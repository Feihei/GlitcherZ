#![forbid(unsafe_code)]

//! ResoZ 桌面应用入口：启动音频引擎（resoz-engine）并打开主窗口
//! （resoz-ui）。当前为路线图第 2 步 —— egui 集成 + 滑块实时控频。

use std::process::ExitCode;
use std::time::Duration;

use resoz_engine::AudioEngine;

/// 命令行参数（手动解析，保持依赖最小）。
struct Args {
    freq_hz: f64,
    gain: f64,
    seconds: Option<f64>,
    list_devices: bool,
    help: bool,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = parse_args(std::env::args().skip(1));
    if args.help {
        print_usage();
        return ExitCode::SUCCESS;
    }
    if args.list_devices {
        println!("可用的输出设备：");
        for name in resoz_engine::available_outputs() {
            println!("  - {name}");
        }
        return ExitCode::SUCCESS;
    }

    let engine = match AudioEngine::start() {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("启动音频引擎失败：{err}");
            return ExitCode::FAILURE;
        }
    };
    let info = engine.info().clone();
    println!(
        "输出设备：{}（{} Hz，{} 声道）",
        info.device_name, info.sample_rate, info.out_channels
    );

    let ui_config = resoz_ui::UiConfig {
        freq_hz: args.freq_hz,
        gain: args.gain,
        auto_close_after: args.seconds.map(|s| Duration::from_secs_f64(s.max(0.0))),
    };

    if let Err(err) = resoz_ui::run(engine, ui_config) {
        eprintln!("主窗口异常退出：{err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn print_usage() {
    println!(
        "用法: resoz [--freq <Hz>] [--gain <0.0-1.0>] [--seconds <秒>] [--list-devices]\n\
         默认：打开主窗口，正弦音源 440 Hz 持续发声，关闭窗口退出。\n\
         --seconds 指定 N 秒后自动关窗（冒烟测试用）。"
    );
}

fn parse_args(raw: impl Iterator<Item = String>) -> Args {
    let mut args = Args {
        freq_hz: 440.0,
        gain: 0.8,
        seconds: None,
        list_devices: false,
        help: false,
    };
    let mut it = raw;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--freq" => {
                if let Some(v) = it.next().and_then(|s| s.parse().ok()) {
                    args.freq_hz = v;
                }
            }
            "--gain" => {
                if let Some(v) = it.next().and_then(|s| s.parse().ok()) {
                    args.gain = v;
                }
            }
            "--seconds" => {
                if let Some(v) = it.next().and_then(|s| s.parse().ok()) {
                    args.seconds = Some(v);
                }
            }
            "--list-devices" => args.list_devices = true,
            "--help" | "-h" => args.help = true,
            other => eprintln!("忽略未知参数：{other}（--help 查看用法）"),
        }
    }
    args
}
