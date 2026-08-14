#!/usr/bin/env node
/**
 * seapack.cjs — Node 22 Single Executable Application (SEA) 打包脚本 (CommonJS)
 *
 * 产出单文件 dsh.exe（内嵌 Node 运行时 + deepseek-harness CLI/WebUI）。
 *
 * ── Tier 1 主路径（本脚本）──
 *   1) node --experimental-sea-config sea-config.json   生成 blob
 *   2) 复制 Node 官方的 sea-tmp.exe（与 node.exe 同级，用 process.execPath 定位）作为宿主
 *   3) npx postject dist/dsh.exe NODE_SEA_BLOB <blob> --sea-config sea-config.json 注入
 *
 * ── Tier 2 兜底（在 README「打包兼容性矩阵」说明）──
 *   若 node-pty / koffi 等原生模块在 SEA 下验证不通过，应改用「侧载 node.exe + node_modules」：
 *   直接把 node.exe 与编译好的 node_modules 随安装包分发，由桌面后端以 `node dsh.js` 启动。
 *   依据开发文档 §12.6 兼容性矩阵选择。
 *
 * 依赖：仅使用 Node 内置 child_process / fs / path，不引入额外 npm 依赖。
 * postject 通过 `npx` 按需拉取。
 */

'use strict';

const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');

const configPath = path.join(__dirname, 'sea-config.json');

function fail(msg, hint) {
  console.error('[seapack] ERROR: ' + msg);
  if (hint) console.error('[seapack] HINT: ' + hint);
  process.exit(1);
}

function main() {
  // 读取 SEA 配置（同目录 sea-config.json）
  if (!fs.existsSync(configPath)) {
    fail('找不到 sea-config.json: ' + configPath, '请确认文件位于 scripts/ 目录。');
  }
  let config;
  try {
    config = JSON.parse(fs.readFileSync(configPath, 'utf8'));
  } catch (e) {
    fail('sea-config.json 解析失败: ' + e.message);
  }

  // main 必须指向构建后的 harness 入口（pnpm run build 产出 apps/cli/lib）
  if (!config.main) {
    fail('sea-config.json 缺少 main 字段（应指向构建后的 harness 入口 js）。');
  }
  const mainPath = path.resolve(__dirname, config.main);
  if (!fs.existsSync(mainPath)) {
    fail('sea-config.json 的 main 指向的文件不存在: ' + mainPath,
      '请先运行 build-harness.ps1 构建 harness（pnpm install && pnpm run build）。');
  }

  const distDir = path.join(__dirname, '..', 'dist');
  const distExe = path.join(distDir, 'dsh.exe');
  fs.mkdirSync(distDir, { recursive: true });

  try {
    // 1) 生成 SEA blob
    console.log('[seapack] 生成 SEA blob (node --experimental-sea-config) ...');
    execSync('node --experimental-sea-config "' + configPath + '"', {
      cwd: __dirname,
      stdio: 'inherit',
      shell: true,
    });

    const blobName = config.output || 'sea-prep.blob';
    const blobPath = path.join(__dirname, blobName);
    if (!fs.existsSync(blobPath)) {
      fail('未生成 blob: ' + blobPath, '检查 sea-config.json 的 output 字段。');
    }

    // 2) 复制宿主可执行文件（Node 官方 sea-tmp.exe，与 node.exe 同级）
    const nodeDir = path.dirname(process.execPath);
    let hostSrc = path.join(nodeDir, 'sea-tmp.exe');
    if (!fs.existsSync(hostSrc)) {
      // 回退：标准 Node SEA 做法——直接用 node.exe 作为宿主
      hostSrc = process.execPath;
      console.warn('[seapack] 未找到 sea-tmp.exe，回退使用 node.exe 作为宿主: ' + hostSrc);
    }
    console.log('[seapack] 复制宿主可执行文件: ' + hostSrc + ' -> ' + distExe);
    fs.copyFileSync(hostSrc, distExe);

    // 3) postject 注入 blob（路径用双引号包裹，兼容含空格路径）
    console.log('[seapack] 注入 NODE_SEA_BLOB ...');
    execSync(
      'npx postject "' + distExe + '" NODE_SEA_BLOB "' + blobPath +
      '" --sea-config "' + configPath + '"',
      { stdio: 'inherit', shell: true }
    );

    console.log('[seapack] 成功产出单文件: ' + distExe);
  } catch (e) {
    fail('SEA 打包失败: ' + (e && e.message ? e.message : e),
      '若原生模块 (node-pty/koffi) 在 SEA 下报错，请改用 Tier 2 侧载 node.exe + node_modules（见 README §12.6）。');
  }
}

main();
