//! 用隔离配置发送一条不影响播放状态的 applog 探针。

use libresoda::soda::media_ref::MediaRef;
use libresoda::soda::user_playlist::fetch_pc_me;
use libresoda::{AppCredentials, ApplogIdentity, Soda};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: applog_probe <config.json>")?;
    let settings: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let cookie = settings["cookie"].as_str().unwrap_or("").to_string();
    let device_id = settings["device_id"].as_str().unwrap_or("").to_string();
    let iid = settings["iid"].as_str().unwrap_or("").to_string();
    let fp = settings["fp"].as_str().unwrap_or("").to_string();
    let _ = settings["signer_url"].as_str().unwrap_or("");
    let _ = settings["signer_token"].as_str().unwrap_or("");

    let soda = Soda::new(cookie);
    soda.set_app_credentials(AppCredentials {
        device_id: device_id.clone(),
        iid,
        fp,
        ..Default::default()
    });
    let user_id = match std::env::var("SODAM_USER_ID") {
        Ok(id) => id.trim().to_string(),
        Err(_) => fetch_pc_me(&soda)?.my_info.id,
    };
    eprintln!(
        "probe identity: user_id_len={} device_id_len={}",
        user_id.len(),
        device_id.len()
    );
    let identity = ApplogIdentity {
        device_id,
        user_id,
        app_version: "3.7.0".to_string(),
        ..ApplogIdentity::from_soda(&soda)
    };
    let media = MediaRef::track("7501674235158431760");
    let result = soda.report_play_progress(&identity, &media, 1.0, 120.0, true)?;
    println!("{}", result);
    Ok(())
}
