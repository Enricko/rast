use std::{error::Error, fs, path::Path};
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cfg = rast::config::Config::load(Some(Path::new("/etc/rast/rast.toml")))?;
    let db = rast::db::init(&cfg).await?;
    let password = fs::read_to_string("/tmp/rast-browser106.password")?;
    let user = rast::users::create(
        &db,
        &cfg,
        "rast-browser106",
        password.trim(),
        rast::auth::Role::Admin,
        Some("Temporary browser 1.0.6"),
        Some(0),
    )
    .await?;
    println!("temporary user id={}", user.id);
    Ok(())
}
