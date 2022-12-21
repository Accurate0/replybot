use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct Config {
    pub discord_token: String,
}
