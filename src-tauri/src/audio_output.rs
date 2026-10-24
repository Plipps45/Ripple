use serde_json::{json, Value};

#[cfg(target_os = "linux")]
async fn sh(cmd: &str) -> Option<String> {
    let fut = tokio::process::Command::new("sh").arg("-c").arg(cmd).kill_on_drop(true).output();
    let out = tokio::time::timeout(std::time::Duration::from_millis(1500), fut).await.ok()?.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(target_os = "linux")]
pub async fn system_audio_output_info() -> Value {
    use regex::Regex;
    let cap = |re: &str, text: &str| Regex::new(re).ok()?.captures(text).and_then(|c| c.get(1)).map(|m| m.as_str().trim().to_string());
    let mut name = String::new();
    let mut bt = String::new();
    if let Some(out) = sh("wpctl inspect @DEFAULT_AUDIO_SINK@ 2>/dev/null").await {
        name = cap(r#"node\.description\s*=\s*"([^"]+)""#, &out).unwrap_or_default();
        bt = cap(r#"api\.bluez5\.address\s*=\s*"([^"]+)""#, &out).unwrap_or_default();
    }
    if name.is_empty() {
        if let Some(out) = sh("pactl info 2>/dev/null && pactl list sinks 2>/dev/null").await {
            if let Some(def) = cap(r"Default Sink:\s*([^\r\n]+)", &out) {
                if let Some(block) = Regex::new(r"Sink #\d+").unwrap().split(&out).find(|b| b.contains(&def)) {
                    name = cap(r"Description:\s*([^\r\n]+)", block).unwrap_or_default();
                }
            }
        }
    }
    let mut battery: Option<i64> = None;
    if let Some(out) = sh("upower -d 2>/dev/null").await {
        for dev in Regex::new(r"Device:\s*").unwrap().split(&out) {
            if dev.contains("headset") || (!bt.is_empty() && dev.contains(&bt.replace(':', "_"))) || (!name.is_empty() && dev.contains(&name)) {
                if let Some(p) = cap(r"percentage:\s*(\d+)%", dev) {
                    battery = p.parse().ok();
                    break;
                }
            }
        }
    }
    if battery.is_none() {
        let cmd = if bt.is_empty() { "bluetoothctl info 2>/dev/null".to_string() } else { format!("bluetoothctl info {bt} 2>/dev/null") };
        if let Some(out) = sh(&cmd).await {
            battery = cap(r"Battery Percentage:\s*(?:0x[0-9a-fA-F]+\s*)?\(?(\d+)\)?", &out).and_then(|p| p.parse().ok());
            if name.is_empty() {
                name = cap(r"Alias:\s*([^\r\n]+)", &out).or_else(|| cap(r"Name:\s*([^\r\n]+)", &out)).unwrap_or_default();
            }
        }
    }
    json!({ "name": if name.is_empty() { "Default output".to_string() } else { name }, "battery": battery })
}

#[cfg(not(target_os = "linux"))]
pub async fn system_audio_output_info() -> Value {
    json!({ "name": "Default output", "battery": null })
}
