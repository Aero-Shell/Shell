// Battery percent from `acpi`; "N/A" when unavailable.
pub fn battery_percent() -> String {
    #[cfg(unix)]
    {
        use std::process::Command;
        if let Ok(out) = Command::new("acpi").args(["-b"]).output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            // "Battery 0: Discharging, 87%"
            if let Some(pos) = text.find('%') {
                let start = text[..pos].rfind(|c: char| c.is_ascii_digit()).unwrap_or(0);
                let chunk = &text[start..=pos];
                if let Ok(num) = chunk
                    .trim()
                    .split('%')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .parse::<u8>()
                {
                    return format!("{num}%");
                }
            }
        }
    }

    "N/A".into()
}
