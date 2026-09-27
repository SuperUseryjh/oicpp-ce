/**
 * 生成 `frontend/renderer/js/oicpp-api-legacy.js` —— 与 `window.electronAPI` 同名的扁平 API 桥。
 *
 * 为什么需要它：
 *   渲染层有 500+ 处 `window.electronAPI.xxx()` 调用。逐个手写改名既慢又容易漏，
 *   而漏一个就会在运行时炸掉一处功能。这里自动生成同名方法，于是迁移退化为一次机械替换：
 *
 *       window.electronAPI.  ->  window.oicppApi.
 *
 * 两类方法的处理方式不同：
 *   1. 能识别出通道名的（shim 里的 makeInvoker('channel')）→ 直接 invoke 该通道，
 *      替换后就不再经过「模拟 Electron」这一层；
 *   2. 识别不出通道的（主要是 onXxx 事件订阅，以及少量手写方法）→ 暂时透传给
 *      shim 的同名实现。这样替换 100% 安全，事件层的迁移留作后续独立工作。
 *
 * 用法：node scripts/gen-api-bridge.mjs（必须在 npm run prepare:frontend 之后执行，
 *       因为产物要写进已准备好的 frontend/ 目录）
 * 产物：frontend/renderer/js/oicpp-api-legacy.js（自动生成，勿手改）
 */
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..');
const shimPath = path.join(root, 'frontend-src', 'shim', 'electron-api.js');
const rendererDir = path.join(root, 'frontend-src', 'renderer');
const prepareDir = path.join(root, 'frontend');
// 产物必须落在页面里的 <script src="js/oicpp-api-legacy.js"> 能指到的位置：
// 页面在 frontend/renderer/ 下，所以是 renderer/js/，不是 frontend/js/。
const outPath = path.join(prepareDir, 'renderer', 'js', 'oicpp-api-legacy.js');

const shim = fs.readFileSync(shimPath, 'utf8');

/// 方法名 -> 通道名
const channels = new Map();
const invokerRe = /^\s*([A-Za-z_$][\w$]*)\s*:\s*makeInvoker\(\s*'([^']+)'/gm;
for (const [, name, channel] of shim.matchAll(invokerRe)) {
    if (!channels.has(name)) channels.set(name, channel);
}
// 手写实现里首行就能看出通道的，同样收进来
const inlineRe = /^\s*([A-Za-z_$][\w$]*)\s*:\s*(?:async\s*)?function\s*\([^)]*\)\s*\{\s*return\s+(?:await\s+)?ipcInvoke\(\s*'([^']+)'/gm;
for (const [, name, channel] of shim.matchAll(inlineRe)) {
    if (!channels.has(name)) channels.set(name, channel);
}

/// 渲染层实际用到的 electronAPI 方法名 —— 只为这些生成，避免暴露无用接口
function collectUsedNames(dir, out = new Set()) {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
        const full = path.join(dir, entry.name);
        if (entry.isDirectory()) {
            collectUsedNames(full, out);
        } else if (/\.(js|html)$/.test(entry.name)) {
            const text = fs.readFileSync(full, 'utf8');
            // 两类都要收：迁移前的写法是 electronAPI，迁移后是 oicppApi。
            // 只扫其中一种的话，替换完成后再重新生成会得到一个几乎空的桥。
            for (const match of text.matchAll(/(?:electronAPI|oicppApi)\??\.([A-Za-z_$][\w$]*)/g)) {
                out.add(match[1]);
            }
        }
    }
    return out;
}

// 手工迁移的 API 分组（见 renderer/js/oicpp-api.js）不归桥管：它们是对象而不是方法，
// 桥若「提供」同名成员会把它们覆盖成函数，反而破坏 dialog.pickFolder / fs.readFile 这类调用
const MANUAL_GROUPS = new Set(['dialog', 'settings', 'http', 'fs', 'send']);

const used = collectUsedNames(rendererDir);
const direct = [];
const pass = [];
for (const name of [...used].sort()) {
    if (MANUAL_GROUPS.has(name)) continue;
    if (channels.has(name)) direct.push([name, channels.get(name)]);
    else pass.push(name);
}

const directBody = direct
    .map(([name, channel]) => `        ${JSON.stringify(name)}: ${JSON.stringify(channel)},`)
    .join('\n');
const passBody = pass.map((name) => `        ${JSON.stringify(name)},`).join('\n');

const output = `/**
 * 自动生成，请勿手改。
 *
 *   来源：frontend-src/shim/electron-api.js 的「方法名 -> 通道名」映射
 *   生成：node scripts/gen-api-bridge.mjs
 *
 * 覆盖渲染层用到的 ${used.size} 个 API 方法：
 *   - ${direct.length} 个已知通道，直接 invoke（不再经过模拟 Electron 层）
 *   - ${pass.length} 个暂时透传给 shim 的同名实现（主要是 onXxx 事件订阅，待独立迁移）
 */
(function () {
    'use strict';

    // 已知通道：直接调用 Rust 侧
    var DIRECT = {
${directBody}
    };

    // 暂时透传（事件订阅等）
    var PASS_THROUGH = [
${passBody}
    ];

    function core() {
        return window.__TAURI__ && window.__TAURI__.core;
    }

    function invoke(channel, args) {
        var api = core();
        if (!api || typeof api.invoke !== 'function') {
            return Promise.reject(new Error('Tauri API 不可用（withGlobalTauri 未开启？）'));
        }
        return api.invoke('ipc_invoke', { channel: channel, args: args || [] });
    }

    window.oicppApi = window.oicppApi || {};

    Object.keys(DIRECT).forEach(function (name) {
        if (typeof window.oicppApi[name] === 'function') return;   // 手工迁移过的优先
        window.oicppApi[name] = function () {
            return invoke(DIRECT[name], Array.prototype.slice.call(arguments));
        };
    });

    PASS_THROUGH.forEach(function (name) {
        if (typeof window.oicppApi[name] === 'function') return;
        window.oicppApi[name] = function () {
            var shim = window.electronAPI;
            if (!shim || typeof shim[name] !== 'function') {
                return Promise.reject(new Error('未实现: ' + name));
            }
            return shim[name].apply(shim, arguments);
        };
    });

    window.oicppApi.__channels = DIRECT;
})();
`;

fs.mkdirSync(path.dirname(outPath), { recursive: true });
fs.writeFileSync(outPath, output, 'utf8');
console.log(
    `[api-bridge] 渲染层用到 ${used.size} 个方法：直接调用 ${direct.length} 个，透传 ${pass.length} 个 -> ${path.relative(root, outPath)}`
);
