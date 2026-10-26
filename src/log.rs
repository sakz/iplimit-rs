pub fn log(message: impl AsRef<str>) {
    println!("[iplimit] {}", message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    eprintln!("[iplimit] 警告：{}", message.as_ref());
}
