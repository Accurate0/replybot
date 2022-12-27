use client::get_http_client_with_headers;
use config::{Config, Environment, File, FileFormat};
use futures::lock::Mutex;
use futures::stream::StreamExt;
use futures::FutureExt;
use http::HeaderMap;
use lazy_static::lazy_static;
use logging::setup_logging;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use reqwest_middleware::ClientWithMiddleware;
use std::{error::Error, sync::Arc};
use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::{Cluster, Event};
use twilight_http::Client as HttpClient;
use twilight_model::channel::message::allowed_mentions::AllowedMentionsBuilder;
use twilight_model::gateway::Intents;
use types::{OpenAICompletionRequest, OpenAICompletionResponse};

mod client;
mod logging;
mod types;

lazy_static! {
    static ref RNG: Arc<Mutex<SmallRng>> = Arc::new(Mutex::new(SmallRng::from_entropy()));
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    setup_logging();
    let config = Config::builder()
        .add_source(File::new("config.json", FileFormat::Json))
        .add_source(Environment::with_prefix("REPLYBOT"))
        .build()?
        .try_deserialize::<types::Config>()?;

    let (cluster, mut events) = Cluster::new(
        config.discord_token.to_owned(),
        Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT,
    )
    .await?;
    let cluster = Arc::new(cluster);

    let cluster_spawn = Arc::clone(&cluster);

    tokio::spawn(async move {
        cluster_spawn.up().await;
    });

    let mut headers = HeaderMap::new();
    headers.insert("X-Api-Key", config.api_key.parse()?);
    headers.insert("Content-Type", "application/json".parse()?);

    let discord_http = Arc::new(HttpClient::new(config.discord_token.to_owned()));
    let http_client = Arc::new(get_http_client_with_headers(headers));

    let cache = InMemoryCache::builder()
        .resource_types(ResourceType::MESSAGE)
        .build();

    let config = Arc::new(config);
    while let Some((shard_id, event)) = events.next().await {
        cache.update(&event);
        tokio::spawn(
            handle_event(
                shard_id,
                event,
                Arc::clone(&discord_http),
                Arc::clone(&http_client),
                Arc::clone(&config),
            )
            .then(|result| async {
                match result {
                    Ok(_) => {}
                    Err(e) => log::error!("{}", e),
                }
            }),
        );
    }

    Ok(())
}

async fn handle_event(
    shard_id: u64,
    event: Event,
    discord: Arc<HttpClient>,
    http: Arc<ClientWithMiddleware>,
    config: Arc<types::Config>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    match event {
        Event::MessageCreate(msg) if !msg.author.bot => {
            let mut rng = RNG.lock().await;
            if rng.gen_bool(config.trigger_chance) {
                log::info!("triggered reply for: {}", msg.author.id);
                discord.create_typing_trigger(msg.channel_id).await?;

                let response = http
                    .post("https://api.anurag.sh/openai/v1/completions")
                    .json(&OpenAICompletionRequest {
                        model: "text-davinci-003".to_owned(),
                        prompt: [config.prompt.to_owned(), msg.content.to_owned()].to_vec(),
                        max_tokens: 32,
                    })
                    .send()
                    .await?
                    .json::<OpenAICompletionResponse>()
                    .await?;

                discord
                    .create_message(msg.channel_id)
                    .reply(msg.id)
                    .fail_if_not_exists(false)
                    .allowed_mentions(Some(&AllowedMentionsBuilder::default().build()))
                    .content(&response.choices.first().unwrap().text)?
                    .await?;
            }
        }
        Event::ShardConnected(_) => {
            log::info!("Connected on shard {shard_id}");
        }
        // Other events here...
        _ => {}
    }

    Ok(())
}
