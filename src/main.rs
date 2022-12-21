use config::{Config, Environment, File, FileFormat};
use futures::stream::StreamExt;
use std::{error::Error, sync::Arc};
use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::{Cluster, Event};
use twilight_http::Client as HttpClient;
use twilight_model::gateway::Intents;

mod types;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    let http = Arc::new(HttpClient::new(config.discord_token));

    let cache = InMemoryCache::builder()
        .resource_types(ResourceType::MESSAGE)
        .build();

    while let Some((shard_id, event)) = events.next().await {
        cache.update(&event);
        tokio::spawn(handle_event(shard_id, event, Arc::clone(&http)));
    }

    Ok(())
}

async fn handle_event(
    shard_id: u64,
    event: Event,
    http: Arc<HttpClient>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    match event {
        Event::MessageCreate(msg) if msg.content == "!ping" => {
            http.create_message(msg.channel_id)
                .content("Pong!")?
                .await?;
        }
        Event::ShardConnected(_) => {
            println!("Connected on shard {shard_id}");
        }
        // Other events here...
        _ => {}
    }

    Ok(())
}
