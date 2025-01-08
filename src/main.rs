use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use directories::BaseDirs;
use inquire::list_option::ListOption;
use inquire::validator::Validation;
use inquire::DateSelect;
use inquire::{Confirm, MultiSelect, Select, Text};
use serde::{Serialize, Deserialize};
use std::process::Command;
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use regex::Regex;

mod celoxis;
use celoxis::{CeloxisApi, CeloxisProject, CeloxisTask, CeloxisTimeEntry};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TimeEntry {
    id: String,
    start: DateTime<Utc>,
    end: Option<DateTime<Utc>>,
    tags: Vec<String>,
    annotation: Option<String>,
    submitted: bool,
    celoxis_id: Option<String>,
}

impl TimeEntry {
    fn from_timewarrior(line: &str, entry_id: String) -> Result<Self, Box<dyn Error>> {
        if line.trim().is_empty() {
            return Err("Empty line".into());
        }

        if !line.starts_with("inc ") {
            return Err("Line doesn't start with 'inc'".into());
        }

        let parts: Vec<&str> = line.splitn(2, "inc ").collect();
        if parts.len() != 2 {
            return Err("Invalid interval format".into());
        }

        let interval_and_tags: Vec<&str> = parts[1].splitn(2, '#').collect();
        let interval = interval_and_tags[0].trim();

        let timestamps: Vec<&str> = interval.split(" - ").collect();
        if timestamps.len() != 2 {
            return Err("Invalid timestamp format".into());
        }

        let start_str = timestamps[0].trim();
        let start = chrono::NaiveDateTime::parse_from_str(start_str, "%Y%m%dT%H%M%SZ")?;
        let start = DateTime::<Utc>::from_naive_utc_and_offset(start, Utc);

        let end = if timestamps[1].trim().is_empty() {
            None
        } else {
            let end_str = timestamps[1].trim();
            let end = chrono::NaiveDateTime::parse_from_str(end_str, "%Y%m%dT%H%M%SZ")?;
            Some(DateTime::<Utc>::from_naive_utc_and_offset(end, Utc))
        };

        let tags = if interval_and_tags.len() > 1 {
            let tag_str = interval_and_tags[1].trim();
            let mut tags = Vec::new();
            let mut current_tag = String::new();
            let mut in_quotes = false;

            for c in tag_str.chars() {
                match c {
                    '"' => {
                        in_quotes = !in_quotes;
                        if !in_quotes && !current_tag.is_empty() {
                            tags.push(current_tag.clone());
                            current_tag.clear();
                        }
                    }
                    ' ' if !in_quotes => {
                        if !current_tag.is_empty() {
                            tags.push(current_tag.clone());
                            current_tag.clear();
                        }
                    }
                    _ => current_tag.push(c),
                }
            }

            if !current_tag.is_empty() {
                tags.push(current_tag);
            }

            tags
        } else {
            Vec::new()
        };

        Ok(TimeEntry {
            id: entry_id,
            start,
            end,
            tags,
            annotation: None,
            submitted: false,
            celoxis_id: None,
        })
    }
}

#[derive(Debug, Clone)]
struct DateRange {
    start: NaiveDate,
    end: NaiveDate,
}

#[derive(Debug, Clone)]
struct GroupedEntry {
    tags: Vec<String>,
    total_duration: HashMap<NaiveDate, i64>, // Duration in minutes per day
    entries: HashMap<NaiveDate, Vec<TimeEntry>>,
    all_submitted: bool,
}

#[derive(Debug)]
struct TaskAssignment {
    groups: Vec<GroupedEntry>,
    total_duration: HashMap<NaiveDate, i64>,
    celoxis_project: CeloxisProject,
    celoxis_task: CeloxisTask,
    summary: String,
    time_code: String,
    user: String,
}

#[derive(Debug)]
struct TimeData {
    entries: Vec<TimeEntry>,
    data_dir: PathBuf,
}

struct CeloxisData {
    api: CeloxisApi,
    cached_projects: Option<Vec<CeloxisProject>>,
    selected_project: Option<CeloxisProject>,
    selected_tasks: Vec<CeloxisTask>,
}

impl CeloxisData {
    fn new() -> Result<Self, Box<dyn Error>> {
        let mut api = CeloxisApi::new()?;
        // Load projects immediately
        let projects = api.get_projects(true)?;

        Ok(Self {
            api,
            cached_projects: Some(projects),
            selected_project: None,
            selected_tasks: Vec::new(),
        })
    }

    fn select_project(&mut self) -> Result<(), Box<dyn Error>> {
        let projects = if let Some(ref projects) = self.cached_projects {
            projects.clone()
        } else {
            let projects = self.api.get_projects(true)?;
            self.cached_projects = Some(projects.clone());
            projects
        };

        let project_options: Vec<String> = projects
            .iter()
            .map(|p| format!("{} - {}", p.id, p.name))
            .collect();

        if let Some(selection) = Select::new(
            "Select project to associate time entries with:",
            project_options.clone(),
        )
        .prompt_skippable()?
        {
            let idx = project_options
                .iter()
                .position(|x| x == &selection)
                .unwrap();
            self.selected_project = Some(projects[idx].clone());
        }

        Ok(())
    }

    fn select_tasks(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(project) = &self.selected_project {
            let force_refresh = if self.api.get_cached_tasks(&project.id).is_some() {
                Confirm::new("Refresh task list from Celoxis?")
                    .with_default(false)
                    .prompt()?
            } else {
                true
            };

            let mut tasks = self.api.get_tasks(&project.id, force_refresh)?;

            tasks.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

            let task_options: Vec<String> = tasks
                .iter()
                .map(|t| format!("{} - {}", t.id, t.name))
                .collect();

            // Changed from MultiSelect to Select
            if let Ok(selection) = Select::new(
                "Select task to associate time entries with:",
                task_options.clone(),
            )
            .prompt()
            {
                let idx = task_options.iter().position(|x| x == &selection).unwrap();
                self.selected_tasks = vec![tasks[idx].clone()];
            }
        }

        Ok(())
    }
}

impl TimeData {
    fn new(date_range: &DateRange) -> Result<Self, Box<dyn Error>> {
        let data_dir = Self::detect_timewarrior_dir()?;
        let entries = Self::read_time_entries(date_range)?;
        
        Ok(TimeData { entries, data_dir })
    }
    fn is_file_in_date_range(filename: &str, range: &DateRange) -> bool {
        // Expected format: YYYY-MM.data
        let re = Regex::new(r"^(\d{4})-(\d{2})\.data$").unwrap();

        if let Some(captures) = re.captures(filename) {
            if let (Some(year_str), Some(month_str)) = (captures.get(1), captures.get(2)) {
                if let (Ok(year), Ok(month)) = (year_str.as_str().parse::<i32>(), month_str.as_str().parse::<u32>()) {
                    let file_date = NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(range.start);
                    let next_month = if month == 12 {
                        NaiveDate::from_ymd_opt(year + 1, 1, 1)
                    } else {
                        NaiveDate::from_ymd_opt(year, month + 1, 1)
                    }.unwrap_or(range.end);

                    // Check if the file's month overlaps with our date range
                    return !(next_month <= range.start || file_date > range.end);
                }
            }
        }
        false
    }

    fn format_timewarrior_date_start(date: NaiveDate) -> String {
        date.format("%Y-%m-%d").to_string()
    }

    fn format_timewarrior_date_end(date: NaiveDate) -> String {
        date.format("%Y-%m-%dT23:59:59").to_string()
    }

    fn detect_timewarrior_dir() -> Result<PathBuf, Box<dyn Error>> {
        println!("Detecting TimeWarrior directory...");

        if let Some(base_dirs) = BaseDirs::new() {
            let xdg_data = base_dirs.data_dir().join("timewarrior");
            println!("Checking XDG path: {:?}", xdg_data);
            if xdg_data.exists() {
                return Ok(xdg_data);
            }
        }

        let legacy_dir = dirs::home_dir()
            .ok_or("Could not determine home directory")?
            .join(".timewarrior");

        println!("Checking legacy path: {:?}", legacy_dir);
        if legacy_dir.exists() {
            return Ok(legacy_dir);
        }

        if let Some(base_dirs) = BaseDirs::new() {
            let xdg_data = base_dirs.data_dir().join("timewarrior");
            fs::create_dir_all(&xdg_data)?;
            println!("Creating XDG directory: {:?}", xdg_data);
            Ok(xdg_data)
        } else {
            Err("Could not determine TimeWarrior data directory".into())
        }
    }

    fn read_time_entries(date_range: &DateRange) -> Result<Vec<TimeEntry>, Box<dyn Error>> {
        let start_str = Self::format_timewarrior_date_start(date_range.start);
        let end_str = Self::format_timewarrior_date_end(date_range.end);
        
        println!("Fetching time entries from {} to {}", start_str, end_str);
        
        let output = Command::new("timew")
            .args(&["export", "from", &start_str, "to", &end_str])
            .output()?;
    
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            return Err(format!("TimeWarrior export failed: {}", error).into());
        }
    
        let json_str = String::from_utf8(output.stdout)?;
        // println!("Raw TimeWarrior output:\n{}", json_str); // Debug output
    
        let entries: Vec<serde_json::Value> = serde_json::from_str(&json_str)?;
        
        let mut time_entries = Vec::new();
        
        for (idx, entry) in entries.into_iter().enumerate() {
            let entry_obj = entry.as_object()
                .ok_or("Invalid entry format")?;
    
            // println!("Processing entry {}: {:?}", idx, entry_obj); // Debug output
    
            let start_str = entry_obj.get("start")
                .and_then(|v| v.as_str())
                .ok_or("Missing start time")?;
            
            //println!("Parsing start time: {}", start_str); // Debug output
            
            // Try multiple date formats
            let start = match DateTime::parse_from_rfc3339(start_str) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(_) => {
                    // Try alternative format that TimeWarrior might be using
                    NaiveDateTime::parse_from_str(start_str, "%Y%m%dT%H%M%SZ")?
                        .and_local_timezone(Utc)
                        .earliest()
                        .ok_or("Could not determine timezone")?
                }
            };
    
            let end = match entry_obj.get("end") {
                Some(end_val) => {
                    let end_str = end_val.as_str()
                        .ok_or("End time not a string")?;
                    //println!("Parsing end time: {}", end_str); // Debug output
                    
                    Some(match DateTime::parse_from_rfc3339(end_str) {
                        Ok(dt) => dt.with_timezone(&Utc),
                        Err(_) => {
                            NaiveDateTime::parse_from_str(end_str, "%Y%m%dT%H%M%SZ")?
                                .and_local_timezone(Utc)
                                .earliest()
                                .ok_or("Could not determine timezone")?
                        }
                    })
                },
                None => None
            };
    
            // Rest of the processing...
            let tags = entry_obj.get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(String::from)
                    .collect())
                .unwrap_or_default();
    
            let annotation = entry_obj.get("annotation")
                .and_then(|v| v.as_str())
                .map(String::from);
    
            time_entries.push(TimeEntry {
                id: format!("export-{}", idx),
                start,
                end,
                tags,
                annotation,
                submitted: false,
                celoxis_id: None,
            });
        }
    
        Ok(time_entries)
    }

    fn to_local_date(utc: DateTime<Utc>) -> NaiveDate {
        utc.with_timezone(&Local).naive_local().date()
    }

    fn filter_by_date_range(&mut self, range: &DateRange) -> Vec<&TimeEntry> {
        // Load entries for the specified date range
        match Self::read_time_entries(range) {
            Ok(entries) => {
                self.entries = entries;
                self.entries.iter().collect()
            },
            Err(e) => {
                eprintln!("Error reading time entries: {}", e);
                Vec::new()
            }
        }
    }

    fn group_entries_by_tags(&self, entries: Vec<&TimeEntry>) -> Vec<GroupedEntry> {
        let mut groups: HashMap<Vec<String>, HashMap<NaiveDate, Vec<&TimeEntry>>> = HashMap::new();

        for entry in entries {
            let sorted_tags = {
                let mut tags = entry.tags.clone();
                tags.sort();
                tags
            };

            let entry_date = Self::to_local_date(entry.start);

            groups
                .entry(sorted_tags)
                .or_insert_with(HashMap::new)
                .entry(entry_date)
                .or_insert_with(Vec::new)
                .push(entry);
        }

        groups
            .into_iter()
            .map(|(tags, date_entries_map)| {
                let mut total_duration = HashMap::new();
                let mut entries = HashMap::new();

                for (date, entries_vec) in date_entries_map.iter() {
                    let duration = entries_vec
                        .iter()
                        .map(|entry| {
                            let end = entry.end.unwrap_or_else(|| Utc::now());
                            (end - entry.start).num_minutes()
                        })
                        .sum();

                    total_duration.insert(*date, duration);
                    entries.insert(*date, entries_vec.iter().map(|&e| e.clone()).collect());
                }

                GroupedEntry {
                    tags,
                    total_duration,
                    entries,
                    all_submitted: date_entries_map
                        .values()
                        .all(|entries| entries.iter().all(|e| e.submitted)),
                }
            })
            .collect()
    }

    fn prompt_date_range() -> Result<DateRange, Box<dyn Error>> {
        let start_date = DateSelect::new("Select start date:").prompt()?;

        let end_date = DateSelect::new("Select end date:")
            .with_min_date(start_date)
            .prompt()?;

        Ok(DateRange {
            start: start_date,
            end: end_date,
        })
    }

    fn display_grouped_entries(grouped_entries: &[GroupedEntry]) {
        for (idx, group) in grouped_entries.iter().enumerate() {
            //println!("\nGroup {}", idx + 1);

            // Extract description and project from tags if available
            let (description, project) =
                group.tags.iter().fold((None, None), |(desc, proj), tag| {
                    if tag.starts_with("description:") {
                        (Some(tag.trim_start_matches("description:")), proj)
                    } else if tag.starts_with("project:") {
                        (desc, Some(tag.trim_start_matches("project:")))
                    } else {
                        (desc, proj)
                    }
                });

            // Display tags based on available information
            //match (description, project) {
            //    (Some(desc), Some(proj)) => {
            //        println!("Description: {} (Project: {})", desc.trim(), proj.trim())
            //    }
            //    (Some(desc), None) => println!("Description: {}", desc.trim()),
            //    (None, Some(proj)) => println!("Project: {}", proj.trim()),
            //    (None, None) => println!("Tags: {:?}", group.tags),
            //}

            // println!("Duration by date:");
            //for (date, duration) in &group.total_duration {
            //    println!(
            //        "  {} - {} hours {} minutes",
            //        date,
            //        duration / 60,
            //        duration % 60
            //    );
            //}
        }
    }

    fn select_multiple_groups(
        grouped_entries: &[GroupedEntry],
    ) -> Result<Vec<&GroupedEntry>, Box<dyn Error>> {
        if grouped_entries.is_empty() {
            println!("No grouped entries found.");
            return Ok(Vec::new());
        }

        let options: Vec<String> = grouped_entries
            .iter()
            .enumerate()
            .map(|(idx, group)| {
                let total_hours: f64 = group.total_duration.values().sum::<i64>() as f64 / 60.0;

                // Extract description and project from tags
                let (description, project) =
                    group.tags.iter().fold((None, None), |(desc, proj), tag| {
                        if tag.starts_with("description:") {
                            (Some(tag.trim_start_matches("description:")), proj)
                        } else if tag.starts_with("project:") {
                            (desc, Some(tag.trim_start_matches("project:")))
                        } else {
                            (desc, proj)
                        }
                    });

                // Format display string based on available information
                let display_info = match (description, project) {
                    (Some(desc), Some(proj)) => {
                        format!("{} (Project: {})", desc.trim(), proj.trim())
                    }
                    (Some(desc), None) => desc.trim().to_string(),
                    (None, Some(proj)) => format!("Project: {}", proj.trim()),
                    (None, None) => format!("Tags: {:?}", group.tags),
                };

                format!(
                    "Group {} - {} - Total: {:.2}h {}",
                    idx + 1,
                    display_info,
                    total_hours,
                    if group.all_submitted {
                        "[Submitted]"
                    } else {
                        ""
                    }
                )
            })
            .collect();

        let selections = MultiSelect::new(
            "Select groups to process (Space to select, Enter to confirm):",
            options.clone(), // Clone here so we can use options later
        )
        .with_validator(|selections: &[ListOption<&String>]| {
            if selections.is_empty() {
                Ok(Validation::Invalid(
                    "Please select at least one group".into(),
                ))
            } else {
                Ok(Validation::Valid)
            }
        })
        .prompt()?;

        Ok(selections
            .iter()
            .filter_map(|selection| {
                let idx = options.iter().position(|x| x == selection)?;
                Some(&grouped_entries[idx])
            })
            .collect())
    }

    fn process_selected_groups(
        groups: Vec<&GroupedEntry>,
    ) -> Result<Vec<GroupedEntry>, Box<dyn Error>> {
        if groups.is_empty() {
            return Err("No groups selected".into());
        }

        let total_minutes: i64 = groups
            .iter()
            .flat_map(|group| group.total_duration.values())
            .sum();

        println!("\nGrouping {} sets of entries", groups.len());
        println!(
            "Total combined duration: {:.2} hours",
            total_minutes as f64 / 60.0
        );

        println!("Including entries with these tags:");
        for group in &groups {
            println!("  - {:?}", group.tags);
        }

        Ok(groups.into_iter().cloned().collect())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let date_range = TimeData::prompt_date_range()?;
    let time_data = TimeData::new(&date_range)?;
    println!("Found {} time entries", time_data.entries.len());
    let mut celoxis = CeloxisData::new()?;

    // Get user preferences once at start
    let user_prefs = celoxis.api.ensure_user_prefs()?;

    // Group entries
    let mut grouped_entries = time_data.group_entries_by_tags(time_data.entries.iter().collect());
    //println!("Grouped into {} sets", grouped_entries.len());

    let mut assignments: Vec<TaskAssignment> = Vec::new();

    // Keep processing until all entries are assigned or user is done
    while !grouped_entries.is_empty() {
        TimeData::display_grouped_entries(&grouped_entries);

        let selected_groups = TimeData::select_multiple_groups(&grouped_entries)?;
        if selected_groups.is_empty() {
            println!("No groups selected. Done assigning.");
            break;
        }

        let processed_groups = TimeData::process_selected_groups(selected_groups.clone())?;

        // Now select project and tasks for these specific entries
        celoxis.select_project()?;
        if let Some(project) = celoxis.selected_project.clone() {
            celoxis.select_tasks()?;

            if celoxis.selected_tasks.is_empty() {
                println!("No tasks selected. Skipping these entries.");
                continue;
            }

            // Get task selection (single task)
            let task = if celoxis.selected_tasks.len() == 1 {
                celoxis.selected_tasks[0].clone()
            } else {
                let task_options: Vec<String> = celoxis
                    .selected_tasks
                    .iter()
                    .map(|t| format!("{} - {}", t.id, t.name))
                    .collect();

                let selected =
                    Select::new("Select the task for these entries:", task_options.clone())
                        .prompt()?;

                let idx = task_options.iter().position(|x| x == &selected).unwrap();
                celoxis.selected_tasks[idx].clone()
            };

            // Get summary for the entries
            let summary = Text::new("Enter work summary for these entries:")
                .with_validator(|input: &str| {
                    if input.trim().is_empty() {
                        Ok(Validation::Invalid("Summary cannot be empty".into()))
                    } else {
                        Ok(Validation::Valid)
                    }
                })
                .prompt()?;

            // Calculate total duration by date
            let mut total_duration = HashMap::new();
            for group in &processed_groups {
                for (date, duration) in &group.total_duration {
                    *total_duration.entry(*date).or_insert(0) += duration;
                }
            }

            // Create the assignment
            let assignment = TaskAssignment {
                groups: processed_groups.clone(),
                total_duration,
                celoxis_project: project,
                celoxis_task: task,
                summary,
                time_code: user_prefs.time_code.clone(),
                user: user_prefs.username.clone(),
            };
            assignments.push(assignment);

            // Collect the tags we need to remove
            let tags_to_remove: Vec<_> = selected_groups.iter().map(|g| g.tags.clone()).collect();

            // Remove the processed groups
            grouped_entries.retain(|group| !tags_to_remove.contains(&group.tags));
        }

        if !grouped_entries.is_empty() {
            let continue_processing = Confirm::new("Assign more entries to tasks?")
                .with_default(true)
                .prompt()?;

            if !continue_processing {
                break;
            }
        }
    }

    // If we have assignments, confirm and process them
    if !assignments.is_empty() {
        println!("\nReady to process {} task assignments", assignments.len());
        println!("\nAssignments to be processed:");
        for assignment in &assignments {
            println!(
                "\nProject: {} (ID: {})",
                assignment.celoxis_project.name, assignment.celoxis_project.id
            );
            println!(
                "Task: {} (ID: {})",
                assignment.celoxis_task.name, assignment.celoxis_task.id
            );
            println!("Duration by date:");
            for (date, duration) in &assignment.total_duration {
                println!("  {} - {:.2} hours", date, *duration as f64 / 60.0);
            }
            println!("Summary: {}", assignment.summary);
            println!("Groups:");
            for group in &assignment.groups {
                println!("  - Tags: {:?}", group.tags);
            }
        }

        let confirm_submit = Confirm::new("Submit all assignments to Celoxis?")
            .with_default(true)
            .prompt()?;

        if confirm_submit {
            let mut all_entries = Vec::new();

            // Collect all entries first
            for assignment in &assignments {
                println!(
                    "\nPreparing entries for project: {} (Task: {})",
                    assignment.celoxis_project.name, assignment.celoxis_task.name
                );

                let celoxis_entries = assignment.to_celoxis_entries();
                for entry in &celoxis_entries {
                    println!(
                        "  {} - {:.2} hours - {}",
                        entry.date, entry.hours, entry.comments
                    );
                }
                all_entries.extend(celoxis_entries);
            }

            println!("\nSubmitting {} total time entries...", all_entries.len());
            match celoxis.api.submit_time_entries(all_entries) {
                Ok(_) => println!("Successfully submitted all entries"),
                Err(e) => println!("Error submitting entries: {}", e),
            }
        } else {
            println!("Submission cancelled.");
        }
    }

    Ok(())
}

impl TaskAssignment {
    fn to_celoxis_entries(&self) -> Vec<CeloxisTimeEntry> {
        let mut celoxis_entries = Vec::new();

        for (date, duration) in &self.total_duration {
            let hours = ((*duration as f64 / 60.0) * 100.0).round() / 100.0; // Round to 2 decimal places

            celoxis_entries.push(CeloxisTimeEntry {
                date: date.format("%Y-%m-%d").to_string(),
                hours,
                time_code: self.time_code.clone(),
                user: self.user.clone(),
                task: self.celoxis_task.id.clone(),
                state: 0,
                comments: self.summary.clone(),
            });
        }

        celoxis_entries
    }
}
