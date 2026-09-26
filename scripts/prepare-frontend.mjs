/**
 * 生成 Tauri 前端产物目录 `frontend/`。
 *
 * 设计说明：
 * - 渲染层源码已复制进本仓库（`frontend-src/`），本脚本只做「复制 + 路径适配」。
 *   `oicpp-origin/` 仅作为上游参考保留（用于对比合并），构建不再依赖它。
 * - 复制过来的渲染层代码保持原样，仅在复制副本上做两类必要的路径/参数适配：
 *   1. `../../node_modules/...` -> `../../vendor/...`（Tauri 不再暴露 node_modules）
 *   2. 设置窗口的 query 参数回退到 `window.__OICPP_QUERY_STRING__`（Tauri 自定义协议不带 search）
 * - 运行期依赖（monaco / xterm / katex / pdfjs）以静态文件形式放入 `frontend/vendor`。
 * - markdown-it / highlight.js / turndown 等原先在主进程（preload）里跑的逻辑，
 *   用 esbuild 打包成 `frontend/shim/markdown-bundle.js`，在渲染进程中直接暴露
 *   `window.markdownAPI` / `window.turndownAPI`。
 */

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as esbuild from 'esbuild';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, '..');
// 渲染层源码在本仓库内（frontend-src/），构建只从这里取。
// `oicpp-origin/` 子模块仅作上游参考（对比合并用），不参与构建。
const SELF_SRC = path.join(ROOT, 'frontend-src');
const OUT = path.join(ROOT, 'frontend');
const VENDOR = path.join(OUT, 'vendor');
const NODE_MODULES = path.join(ROOT, 'node_modules');

const log = (...args) => console.log('[prepare-frontend]', ...args);

function ensureDir(dir) {
  fs.mkdirSync(dir, { recursive: true });
}

function rmrf(target) {
  fs.rmSync(target, { recursive: true, force: true });
}

function copyFile(src, dest) {
  if (!fs.existsSync(src)) {
    throw new Error(`缺少必要文件: ${src}（请确认已 checkout oicpp-origin 子模块并执行 npm install）`);
  }
  ensureDir(path.dirname(dest));
  fs.copyFileSync(src, dest);
}

function copyDir(src, dest) {
  if (!fs.existsSync(src)) {
    throw new Error(`缺少必要目录: ${src}（请确认已 checkout oicpp-origin 子模块并执行 npm install）`);
  }
  fs.cpSync(src, dest, { recursive: true });
}

const TEXT_EXT = new Set(['.html', '.js', '.mjs', '.css', '.json']);

/** 递归遍历目录下所有文件 */
function walk(dir, visit) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(full, visit);
    else if (entry.isFile()) visit(full);
  }
}

/** 对复制到 frontend 的文本文件做替换 */
function rewriteIn(dir, replacements) {
  walk(dir, (file) => {
    if (!TEXT_EXT.has(path.extname(file).toLowerCase())) return;
    let content;
    try {
      content = fs.readFileSync(file, 'utf8');
    } catch {
      return;
    }
    let next = content;
    for (const [pattern, replacement] of replacements) {
      next = next.replace(pattern, replacement);
    }
    if (next !== content) {
      fs.writeFileSync(file, next, 'utf8');
    }
  });
}

function assertExists(target, what) {
  if (!fs.existsSync(target)) {
    throw new Error(`${what} 不存在: ${target}`);
  }
}

async function main() {
  assertExists(path.join(SELF_SRC, 'renderer'), 'frontend-src/renderer');
  assertExists(NODE_MODULES, 'node_modules');

  log('清理输出目录');
  rmrf(OUT);
  ensureDir(OUT);

  // ---------------------------------------------------------------- 1. 渲染层
  log('复制渲染层 / 语言包 / 工具脚本');
  copyDir(path.join(SELF_SRC, 'renderer'), path.join(OUT, 'renderer'));
  copyDir(path.join(SELF_SRC, 'lang'), path.join(OUT, 'lang'));
  ensureDir(path.join(OUT, 'utils'));
  copyFile(path.join(SELF_SRC, 'utils', 'font-detector.js'), path.join(OUT, 'utils', 'font-detector.js'));

  // 根目录静态资源（图标 / 版本 / PDF 文档）
  copyFile(path.join(SELF_SRC, 'oicpp.ico'), path.join(OUT, 'oicpp.ico'));
  copyFile(path.join(SELF_SRC, 'version.txt'), path.join(OUT, 'version.txt'));
  if (fs.existsSync(path.join(SELF_SRC, 'sampleTesterApiDoc.pdf'))) {
    copyFile(path.join(SELF_SRC, 'sampleTesterApiDoc.pdf'), path.join(OUT, 'sampleTesterApiDoc.pdf'));
  }

  // ---------------------------------------------------------------- 2. vendor
  log('复制运行期依赖到 vendor/');
  copyDir(path.join(NODE_MODULES, 'monaco-editor', 'min'), path.join(VENDOR, 'monaco-editor', 'min'));
  copyDir(path.join(NODE_MODULES, 'katex', 'dist'), path.join(VENDOR, 'katex', 'dist'));
  copyDir(path.join(NODE_MODULES, 'xterm', 'lib'), path.join(VENDOR, 'xterm', 'lib'));
  copyDir(path.join(NODE_MODULES, 'xterm', 'css'), path.join(VENDOR, 'xterm', 'css'));
  copyDir(path.join(NODE_MODULES, 'xterm-addon-fit', 'lib'), path.join(VENDOR, 'xterm-addon-fit', 'lib'));
  copyDir(
    path.join(NODE_MODULES, '@xterm', 'addon-unicode11', 'lib'),
    // 目录名保持 `@xterm/addon-unicode11`，否则上面 `../../node_modules/` →
    // `../../vendor/` 的重写会指向不存在的路径（index.html:685 引用的就是这个）
    path.join(VENDOR, '@xterm', 'addon-unicode11', 'lib')
  );
  copyDir(path.join(NODE_MODULES, 'pdfjs-dist', 'build'), path.join(VENDOR, 'pdfjs-dist', 'build'));
  copyDir(path.join(NODE_MODULES, 'pdfjs-dist', 'web'), path.join(VENDOR, 'pdfjs-dist', 'web'));

  // ---------------------------------------------------------------- 3. 路径适配
  log('适配 node_modules 引用路径');
  rewriteIn(path.join(OUT, 'renderer'), [
    // node_modules -> vendor 的路径改写已固化到 frontend-src/renderer 源码里（一次性完成），
    // 这里不再改写：同一件事只有一个地方负责，避免两层改写互相掩盖问题
  ]);

  // pdfjs 的 worker / web viewer 在 vendor 下的相对结构保持一致，无需额外改动

  // ---------------------------------------------------------------- 4. 语言包索引
  log('生成语言包索引 lang/languages.json');
  const langDir = path.join(OUT, 'lang');
  const languages = [];
  for (const file of fs.readdirSync(langDir)) {
    if (!file.endsWith('.json') || file === 'languages.json') continue;
    try {
      const parsed = JSON.parse(fs.readFileSync(path.join(langDir, file), 'utf8'));
      const meta = parsed.meta || {};
      if (meta.code) {
        languages.push({ code: meta.code, name: meta.name || meta.code, nameEn: meta.nameEn || meta.code });
      }
    } catch (err) {
      console.warn('[prepare-frontend] 跳过无法解析的语言包:', file, err.message);
    }
  }
  fs.writeFileSync(path.join(langDir, 'languages.json'), JSON.stringify(languages, null, 2), 'utf8');

  // ---------------------------------------------------------------- 5. CSP 适配
  // Tauri 的 IPC 需要 connect-src 允许 ipc: / http://ipc.localhost，
  // 图片预览走 asset 协议，需要额外放开 asset: / http://asset.localhost。
  log('适配页面内容安全策略');
  const TAURI_CSP = [
    "default-src 'self' ipc: http://ipc.localhost",
    "script-src 'self' 'unsafe-inline' 'unsafe-eval' 'wasm-unsafe-eval'",
    "style-src 'self' 'unsafe-inline'",
    "img-src 'self' data: blob: file: http: https: asset: http://asset.localhost",
    "font-src 'self' data:",
    "worker-src 'self' blob: file: tauri: http://tauri.localhost asset: http://asset.localhost",
    "connect-src 'self' ipc: http://ipc.localhost https: http: asset: http://asset.localhost",
    "frame-src 'self' https: http: data: blob:",
    "media-src 'self' data: blob: https: http:",
  ].join('; ') + ';';
  rewriteIn(path.join(OUT, 'renderer'), [
    [/<meta\s+http-equiv="Content-Security-Policy"\s+content="[^"]*"\s*\/?>/gi,
      `<meta http-equiv="Content-Security-Policy" content="${TAURI_CSP}">`],
  ]);

  // ---------------------------------------------------------------- 6. 设置窗口 query 回退
  log('注入设置窗口 query 参数回退');
  const queryRe = /new URLSearchParams\(\s*window\.location\.search\s*\)/g;
  rewriteIn(path.join(OUT, 'renderer', 'settings'), [
    [queryRe, 'new URLSearchParams(window.__OICPP_QUERY_STRING__ || window.location.search)'],
  ]);

  // ---------------------------------------------------------------- 7. shim 注入
  log('注入 shim 脚本引用');
  const indexPath = path.join(OUT, 'renderer', 'index.html');
  let indexHtml = fs.readFileSync(indexPath, 'utf8');
  const anchor = '<script src="js/electron-helper.js"></script>';
  if (!indexHtml.includes(anchor)) {
    throw new Error('index.html 中未找到 electron-helper.js 引用锚点，无法注入 shim');
  }
  if (!indexHtml.includes('shim/markdown-bundle.js')) {
    indexHtml = indexHtml.replace(anchor, `${anchor}\n    <script src="shim/markdown-bundle.js"></script>`);
    fs.writeFileSync(indexPath, indexHtml, 'utf8');
  }

  // ---------------------------------------------------------------- 8. markdown / turndown 打包
  // 产物必须落在 `renderer/shim/` 下：注入的 script 标签在 renderer/index.html 里，
  // 写绝对路径 `shim/markdown-bundle.js` 会被解析成 `/renderer/shim/...`
  log('打包 markdown-bundle.js');
  ensureDir(path.join(OUT, 'renderer', 'shim'));
  await esbuild.build({
    entryPoints: [path.join(ROOT, 'frontend-src', 'shim', 'markdown-entry.js')],
    outfile: path.join(OUT, 'renderer', 'shim', 'markdown-bundle.js'),
    bundle: true,
    format: 'iife',
    platform: 'browser',
    target: ['chrome105', 'safari15'],
    minify: true,
    legalComments: 'none',
    logLevel: 'warning',
  });

  log('完成，输出目录:', OUT);
}

main().catch((err) => {
  console.error('[prepare-frontend] 失败:', err);
  process.exit(1);
});
