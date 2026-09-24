use std::sync::Arc;

use my_sqweel::{QueryHookEvent, QueryHookOptions, sql::engine::Engine};
use tokio::sync::Notify;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let engine = Engine::default();
    let delivered = Arc::new(Notify::new());
    let completion = delivered.clone();
    let mut subscription = engine.subscribe_query_hooks(
        QueryHookOptions {
            table_created: true,
            table_updated: true,
            table_dropped: true,
            ..Default::default()
        },
        move |event| {
            let completion = completion.clone();
            async move {
                // Await your integration's HTTP/queue client here. Returning an
                // error stops this subscription; the database commit still succeeds.
                println!("{event:?}");
                if matches!(&*event, QueryHookEvent::Write { .. }) {
                    completion.notify_one();
                }
                Ok(())
            }
        },
    )?;
    engine.execute_sql(
        "CREATE TABLE items (id INT PRIMARY KEY, name TEXT); \
         INSERT INTO items VALUES (1, 'example')",
    )?;
    delivered.notified().await;
    subscription.cancel();
    subscription.wait().await?;
    Ok(())
}
