fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = iplimit::cli::run(args) {
        eprintln!("[iplimit] 错误：{error}");
        std::process::exit(1);
    }
}
