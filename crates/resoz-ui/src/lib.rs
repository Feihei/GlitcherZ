#![forbid(unsafe_code)]

//! # resoz-ui
//!
//! ResoZ 的 egui 界面：走带控制、步进音序器、钢琴卷帘、混音器。
//!
//! 当前为骨架 —— 路线图第 2 步将在此接入 `eframe`，并通过
//! `resoz-engine` 的无锁命令队列（`EngineCommand`）把参数改动实时
//! 送往音频线程，用 `AtomicF32` 回传绘制电平表。
