// 预处理指令：release 下隐藏控制台窗口（Windows）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    deepseek_harness_desktop::run();
}
