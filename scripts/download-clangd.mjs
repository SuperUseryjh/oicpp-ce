/**
 * 下载 clangd 到 `src-tauri/resources/clangd/`，随安装包一起分发。
 *
 * 对应 oicpp-origin 的 `scripts/download-clangd.js`。原项目在打包前把 clangd 放进
 * `resources/clangd`，这一步不能省：渲染层的 clangd 客户端在启动失败时会直接抛错
 * （`lsp-client.js`: `startResult.ok !== true` 即 throw），而该调用发生在
 * `MonacoEditorManager.createNewEditor` 内部 —— clangd 缺失会导致**整个编辑器创建失败**，
 * 表现为编辑器区域一直空白。
 *
 * 用法：
 *   node scripts/download-clangd.mjs
 *   node scripts/download-clangd.mjs --version 22.1.0 --platform win32
 * 环境变量：CLANGD_VERSION / CLANGD_TAG / CLANGD_REPO / OICPP_CLANGD_PLATFORM
 */
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, '..');
const OUT = path.join(ROOT, 'src-tauri', 'resources', 'clangd');

const args = process.argv.slice(2);
const readArg = (name, fallback) => {
    const index = args.findIndex((value) => value === `--${name}`);
    return index >= 0 && index + 1 < args.length ? args[index + 1] : fallback;
};

const VERSION = readArg('version', process.env.CLANGD_VERSION || '22.1.0');
const TAG = readArg('tag', process.env.CLANGD_TAG || VERSION);
const REPO = readArg('repo', process.env.CLANGD_REPO || 'clangd/clangd');

// 与 oicpp-origin 的 download-clangd.js 保持一致的资源名匹配规则
const PATTERNS = {
    win32: [/^clangd-windows-.*\.zip$/i, /^clangd-win.*\.zip$/i],
    darwin: [/^clangd-mac-.*\.zip$/i, /^clangd-macos-.*\.zip$/i],
    linux: [/^clangd-linux-.*\.zip$/i]
};

function platformKey() {
    const raw = String(readArg('platform', process.env.OICPP_CLANGD_PLATFORM) || process.platform).toLowerCase();
    if (raw === 'win' || raw === 'windows' || raw === 'win32') return 'win32';
    if (raw === 'mac' || raw === 'macos' || raw === 'darwin') return 'darwin';
    return 'linux';
}

async function findAssetUrl(patterns) {
    const response = await fetch(`https://api.github.com/repos/${REPO}/releases/tags/${TAG}`, {
        headers: {
            'User-Agent': 'oicpp-clangd-downloader',
            Accept: 'application/vnd.github+json'
        }
    });
    if (!response.ok) {
        throw new Error(`GitHub API ${response.status}: ${(await response.text()).slice(0, 200)}`);
    }
    const release = await response.json();
    const assets = Array.isArray(release.assets) ? release.assets : [];
    for (const pattern of patterns) {
        const asset = assets.find((item) => pattern.test(item.name));
        if (asset) {
            return asset.browser_download_url;
        }
    }
    throw new Error(`未找到匹配的 clangd 资源，可用: ${assets.map((item) => item.name).join(', ')}`);
}

function extract(archive, destination) {
    fs.mkdirSync(destination, { recursive: true });
    if (process.platform === 'win32') {
        // tar(bsdtar) 自 Win10 起内置，解 zip 比 Expand-Archive 快得多
        try {
            execFileSync('tar', ['-xf', archive, '-C', destination], { stdio: 'inherit' });
            return;
        } catch (_) {
            execFileSync('powershell', [
                '-NoProfile',
                '-NonInteractive',
                '-Command',
                `Expand-Archive -LiteralPath '${archive}' -DestinationPath '${destination}' -Force`
            ], { stdio: 'inherit' });
            return;
        }
    }
    execFileSync('unzip', ['-o', archive, '-d', destination], { stdio: 'inherit' });
}

function findExecutables(dir) {
    const found = [];
    const walk = (current) => {
        for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
            const full = path.join(current, entry.name);
            if (entry.isDirectory()) {
                walk(full);
            } else if (entry.name === 'clangd' || entry.name === 'clangd.exe') {
                found.push(full);
            }
        }
    };
    walk(dir);
    return found;
}

const platform = platformKey();
const patterns = PATTERNS[platform] || PATTERNS.linux;
console.log(`[clangd] 目标平台 ${platform}，版本 ${TAG}（${REPO}）`);

const url = await findAssetUrl(patterns);
console.log(`[clangd] 下载 ${url}`);
const response = await fetch(url, { headers: { 'User-Agent': 'oicpp-clangd-downloader' } });
if (!response.ok) {
    throw new Error(`下载失败 ${response.status} ${response.statusText}`);
}
const buffer = Buffer.from(await response.arrayBuffer());
console.log(`[clangd] 已下载 ${(buffer.length / 1024 / 1024).toFixed(1)} MB`);

const archive = path.join(os.tmpdir(), `oicpp-clangd-${platform}-${TAG}.zip`);
fs.writeFileSync(archive, buffer);
fs.rmSync(OUT, { recursive: true, force: true });
console.log(`[clangd] 解压到 ${OUT}`);
extract(archive, OUT);
fs.rmSync(archive, { force: true });

const executables = findExecutables(OUT);
if (executables.length === 0) {
    throw new Error('解压后未找到 clangd 可执行文件');
}
console.log('[clangd] 完成:', executables[0]);
