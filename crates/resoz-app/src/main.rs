#![forbid(unsafe_code)]

//! ResoZ 桌面应用入口。
//!
//! 当前实现路线图第 1 步：通过 cpal 输出 440 Hz 正弦波，验证音频管线
//! 与 UI→音频 的无锁命令队列。GUI（egui）在路线图第 2 步接入。

use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use resoz_engine::AudioEngine;
use resoz_engine::convert;
use resoz_engine::realtime::{EngineCommand, ParamId};

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

    let mut engine = match AudioEngine::start() {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("启动音频引擎失败：{err}");
            return ExitCode::FAILURE;
        }
    };

    let info = engine.info().clone();
    println!(
        "输出设备：{}（{} Hz，{} 声道，固定块 128 样本）",
        info.device_name, info.sample_rate, info.out_channels
    );

    // 经无锁命令队列下发参数，验证 UI → 音频线程的通信路径
    // （音频线程在下一个块边界应用）。
    engine.send(EngineCommand::SetParam {
        id: ParamId::SineFrequency,
        value: convert::to_f32(args.freq_hz),
    });
    engine.send(EngineCommand::SetParam {
        id: ParamId::MasterGain,
        value: convert::to_f32(args.gain),
    });

    match args.seconds {
        Some(seconds) => {
            let seconds = seconds.max(0.0);
            println!("播放 {seconds:.2} 秒 {:.1} Hz 正弦波…", args.freq_hz);
            sleep(Duration::from_secs_f64(seconds));
        }
        None => {
            println!("正在播放 {:.1} Hz 正弦波，按 Ctrl+C 退出。", args.freq_hz);
            loop {
                sleep(Duration::from_secs(1));
                println!(
                    "  峰值电平 {:.3}　播放位置 {:.1} s",
                    engine.peak_level(),
                    engine.position_seconds()
                );
            }
        }
    }

    println!("结束。峰值电平 {:.3}", engine.peak_level());
    ExitCode::SUCCESS
}

fn print_usage() {
    println!(
        "用法: resoz [--freq <Hz>] [--gain <0.0-1.0>] [--seconds <秒>] [--list-devices]\n\
         默认：440 Hz 正弦波持续播放，Ctrl+C 退出。"
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
