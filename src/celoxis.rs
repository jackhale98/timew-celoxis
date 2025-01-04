use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use reqwest::blocking::Client;
use reqwest::header;
use directories::BaseDirs;
use inquire::{self, validator::Validation};

const BASE_URL: &str = "https://app.celoxis.com/psa/api/v2";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPreferences {
    pub username: String,
    pub time_code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CeloxisProject {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CeloxisTask {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct CeloxisTimeEntry {
    pub date: String,
    pub hours: f64,
    #[serde(rename = "timeCode")]
    pub time_code: String,
    pub user: String,
    pub task: String,
    pub state: i32,
    pub comments: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CeloxisResponse<T> {
    data: Vec<T>,
    total_records: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CacheData {
    projects: HashMap<String, CeloxisProject>,
    tasks: HashMap<String, Vec<CeloxisTask>>,
    last_updated: DateTime<Utc>,
    user_prefs: Option<UserPreferences>,
}

pub struct CeloxisApi {
    client: Client,
    cache_path: PathBuf,
    cache: Option<CacheData>,
}

impl CeloxisApi {
    fn ensure_api_key_exists() -> Result<(), Box<dyn Error>> {
        if !Path::new("key.txt").exists() {
            println!("API key file (key.txt) not found.");
            println!("Please enter your Celoxis API key:");
            let api_key = inquire::Text::new("API Key:")
                .with_validator(|input: &str| {
                    if input.trim().is_empty() {
                        Ok(Validation::Invalid("API key cannot be empty".into()))
                    } else {
                        Ok(Validation::Valid)
                    }
                })
                .prompt()?;
            fs::write("key.txt", api_key)?;
            println!("API key saved to key.txt");
        }
        Ok(())
    }

    pub fn ensure_user_prefs(&mut self) -> Result<UserPreferences, Box<dyn Error>> {
        if let Some(cache) = &self.cache {
            if let Some(prefs) = &cache.user_prefs {
                return Ok(prefs.clone());
            }
        }

        println!("User preferences not found. Please enter your information:");

        let username = inquire::Text::new("Celoxis Username:")
            .with_validator(|input: &str| {
                if input.trim().is_empty() {
                    Ok(Validation::Invalid("Username cannot be empty".into()))
                } else {
                    Ok(Validation::Valid)
                }
            })
            .prompt()?;

        let time_code = inquire::Text::new("Default Time Code (e.g., engineering_labor):")
            .with_validator(|input: &str| {
                if input.trim().is_empty() {
                    Ok(Validation::Invalid("Time code cannot be empty".into()))
                } else {
                    Ok(Validation::Valid)
                }
            })
            .prompt()?;

        let prefs = UserPreferences {
            username,
            time_code,
        };

        // Update cache with new preferences
        if let Some(cache) = &mut self.cache {
            cache.user_prefs = Some(prefs.clone());
            self.save_cache()?;
        }

        Ok(prefs)
    }

    fn ensure_directories_exist(cache_path: &Path) -> Result<(), Box<dyn Error>> {
        if let Some(parent) = cache_path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    pub fn new() -> Result<Self, Box<dyn Error>> {
        Self::ensure_api_key_exists()?;

        let api_key = fs::read_to_string("key.txt")?;
        let mut headers = header::HeaderMap::new();
        headers.insert(
            "Authorization",
            header::HeaderValue::from_str(&format!("bearer {}", api_key.trim()))?,
        );
        headers.insert(
            "Content-Type",
            header::HeaderValue::from_static("application/json"),
        );

        let client = Client::builder()
            .default_headers(headers)
            .build()?;

        let cache_path = if let Some(base_dirs) = BaseDirs::new() {
            if Path::new(&format!("{}/.local/share/timewarrior", env!("HOME"))).exists() {
                PathBuf::from(format!("{}/.local/share/timewarrior/celoxis_cache.json", env!("HOME")))
            } else if Path::new(&format!("{}/.timewarrior", env!("HOME"))).exists() {
                PathBuf::from(format!("{}/.timewarrior/celoxis_cache.json", env!("HOME")))
            } else {
                PathBuf::from(format!("{}/.local/share/timewarrior/celoxis_cache.json", env!("HOME")))
            }
        } else {
            PathBuf::from("celoxis_cache.json")
        };

        Self::ensure_directories_exist(&cache_path)?;

        let mut api = Self {
            client,
            cache_path,
            cache: None,
        };
        api.load_cache()?;

        Ok(api)
    }

    fn load_cache(&mut self) -> Result<(), Box<dyn Error>> {
        if self.cache_path.exists() {
            let cache_content = fs::read_to_string(&self.cache_path)?;
            self.cache = Some(serde_json::from_str(&cache_content)?);
        } else {
            self.cache = Some(CacheData {
                projects: HashMap::new(),
                tasks: HashMap::new(),
                last_updated: Utc::now(),
                user_prefs: None,
            });
        }
        Ok(())
    }

    fn save_cache(&self) -> Result<(), Box<dyn Error>> {
        if let Some(cache) = &self.cache {
            if let Some(parent) = self.cache_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(
                &self.cache_path,
                serde_json::to_string_pretty(cache)?,
            )?;
        }
        Ok(())
    }

    pub fn get_projects(&mut self, force_refresh: bool) -> Result<Vec<CeloxisProject>, Box<dyn Error>> {
        if !force_refresh {
            if let Some(cache) = &self.cache {
                return Ok(cache.projects.values().cloned().collect());
            }
        }

        let params = [("filter", "{state : Active}")];
        let response: CeloxisResponse<CeloxisProject> = self.client
            .get(&format!("{}/projects", BASE_URL))
            .query(&params)
            .send()?
            .json()?;

        if let Some(cache) = &mut self.cache {
            cache.projects.clear();
            for project in &response.data {
                cache.projects.insert(project.id.clone(), project.clone());
            }
            cache.last_updated = Utc::now();
            self.save_cache()?;
        }

        Ok(response.data)
    }

    pub fn get_tasks(&mut self, project_id: &str, force_refresh: bool)
        -> Result<Vec<CeloxisTask>, Box<dyn Error>>
    {
        if !force_refresh {
            if let Some(cache) = &self.cache {
                if let Some(tasks) = cache.tasks.get(project_id) {
                    return Ok(tasks.clone());
                }
            }
        }

        let filter_json = format!("{{\"project.id\":\"{}\"}}", project_id);
        println!("Fetching tasks with filter: {}", filter_json);

        let params = [("filter", filter_json)];
        let response: CeloxisResponse<CeloxisTask> = self.client
            .get(&format!("{}/tasks", BASE_URL))
            .query(&params)
            .send()?
            .json()?;

        if let Some(cache) = &mut self.cache {
            cache.tasks.insert(project_id.to_string(), response.data.clone());
            cache.last_updated = Utc::now();
            self.save_cache()?;
        }

        Ok(response.data)
    }

    pub fn get_cached_project(&self, project_id: &str) -> Option<&CeloxisProject> {
        self.cache.as_ref()?.projects.get(project_id)
    }

    pub fn get_cached_tasks(&self, project_id: &str) -> Option<&Vec<CeloxisTask>> {
        self.cache.as_ref()?.tasks.get(project_id)
    }

    pub fn submit_time_entries(&self, entries: Vec<CeloxisTimeEntry>) -> Result<(), Box<dyn Error>> {
        let url = format!("{}/timeEntries", BASE_URL);

        let response = self.client
            .post(&url)
            .json(&entries)
            .send()?;

        if !response.status().is_success() {
            let error_json = response.json::<serde_json::Value>()?;
            return Err(format!("Failed to submit time entries: {:?}", error_json).into());
        }

        Ok(())
    }
}
