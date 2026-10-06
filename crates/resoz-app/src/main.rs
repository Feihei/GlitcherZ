#![forbid(unsafe_code)]

//! ResoZ 桌面应用入口：启动音频引擎（resoz-engine）并打开主窗口
//! （resoz-ui）。当前为路线图第 3 步 —— 完整合成器（ADSR 包络 +
//! SVF 滤波器）与虚拟键盘。

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use resoz_engine::AudioEngine;

/// 命令行参数（手动解析，保持依赖最小）。
struct Args {
    /// 启动自动触发的 MIDI 音符（69 = A4）。
    note: u8,
    /// 启动静默（不自动触发音符）。
    no_autoplay: bool,
    /// 启动时加载的项目文件。
    open: Option<PathBuf>,
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
        gain: args.gain,
        auto_play_note: (!args.no_autoplay).then_some(args.note),
        auto_close_after: args.seconds.map(|s| Duration::from_secs_f64(s.max(0.0))),
        open_path: args.open,
    };

    if let Err(err) = resoz_ui::run(engine, ui_config) {
        eprintln!("主窗口异常退出：{err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn print_usage() {
    println!(
        "用法: resoz [--note <0-127>] [--no-autoplay] [--gain <0.0-1.0>] [--seconds <秒>] [--list-devices]\n\
         默认：打开主窗口并自动触发 A4 音符（ sustain 长音），\n\
         用屏幕键盘或电脑键盘（A W S E D …，Z/X 切八度）演奏，关闭窗口退出。\n\
         --seconds 指定 N 秒后自动关窗（冒烟测试用）。"
    );
}

fn parse_args(raw: impl Iterator<Item = String>) -> Args {
    let mut args = Args {
        note: 69,
        no_autoplay: false,
        open: None,
        gain: 0.8,
        seconds: None,
        list_devices: false,
        help: false,
    };
    let mut it = raw;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--note" => {
                if let Some(v) = it.next().and_then(|s| s.parse::<i32>().ok()) {
                    args.note = v.clamp(0, 127) as u8;
                }
            }
            "--freq" => {
                // 兼容旧参数：把频率换算成最近的 MIDI 音符。
                if let Some(f) = it.next().and_then(|s| s.parse::<f64>().ok()) {
                    if f.is_finite() && f > 0.0 {
                        let note = (69.0 + 12.0 * (f / 440.0).log2()).round() as i32;
                        args.note = note.clamp(0, 127) as u8;
                    }
                }
            }
            "--no-autoplay" => args.no_autoplay = true,
            "--open" => {
                if let Some(v) = it.next() {
                    args.open = Some(PathBuf::from(v));
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
