use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct Config {
    pub discord_token: String,
    pub trigger_chance: f64,
    pub api_key: String,
    pub prompt: String,
}
