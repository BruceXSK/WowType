//! `wowtype-sync`：跨设备剪贴板的配对命令行，设置界面调它。用法见 `wowtype_sync::cli`。

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(wowtype_sync::cli::run(&args, &mut std::io::stdout()));
}
