/**
 * 迁移守门检查：渲染层对 `electronAPI` / `oicppApi` 的调用是否都有实现。
 *
 * 两项检查：
 *   1. 还残留的 `electronAPI.<方法>` 访问必须仍被桥覆盖（迁移期的兜底路径没被破坏）；
 *   2. 新的 `oicppApi.<方法>` 访问必须有来源 —— 要么在桥里（DIRECT / PASS_THROUGH），
 *      要么属于手工迁移的 API 分组。这条用于防止「改了个不存在的 API」这类静默失败。
 *
 * 用法：bun scripts/check-api-coverage.mjs
 *       退出码 1 表示有缺失（已接进 CI，会卡住构建）
 */
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const rendererDir = path.join(root, 'frontend-src', 'renderer');
const bridgePath = path.join(root, 'frontend', 'js', 'oicpp-api-legacy.js');

/// 手工迁移的 API 分组（见 frontend-src/renderer/js/oicpp-api.js），这些不是桥提供的
const MANUAL_GROUPS = new Set(['dialog', 'settings', 'http', 'fs', 'send']);

function walk(dir, out = []) {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
        const full = path.join(dir, entry.name);
        if (entry.isDirectory()) {
            walk(full, out);
        } else if (/\.(js|html)$/.test(entry.name)) {
            out.push(full);
        }
    }
    return out;
}

function collect(pattern) {
    const found = new Map(); // 名称 -> 文件集合
    for (const file of walk(rendererDir)) {
        const text = fs.readFileSync(file, 'utf8');
        for (const match of text.matchAll(pattern)) {
            const name = match[1];
            if (!found.has(name)) found.set(name, new Set());
            found.get(name).add(path.relative(root, file));
        }
    }
    return found;
}

if (!fs.existsSync(bridgePath)) {
    console.error(`[coverage] 找不到 ${path.relative(root, bridgePath)}，先跑 scripts/gen-api-bridge.mjs`);
    process.exit(1);
}
const bridge = fs.readFileSync(bridgePath, 'utf8');
// DIRECT 表是 "name": "channel"，透传数组是 "name",（后面没有冒号）—— 两类都算「可提供」
const provided = new Set([
    ...[...bridge.matchAll(/^\s*"([A-Za-z_$][\w$]*)":/gm)].map((m) => m[1]),
    ...[...bridge.matchAll(/^\s*"([A-Za-z_$][\w$]*)",\s*$/gm)].map((m) => m[1])
]);

// 1) 仍以 electronAPI 访问的（可选链也算）
const legacy = collect(/electronAPI\??\.([A-Za-z_$][\w$]*)/g);
const legacyMissing = [...legacy.keys()].filter((name) => !provided.has(name)).sort();

// 2) 新的 oicppApi 访问（取顶层名，例如 fs / dialog / readFileContent）
const modern = collect(/oicppApi\??\.([A-Za-z_$][\w$]*)/g);
const modernMissing = [...modern.keys()]
    .filter((name) => !provided.has(name) && !MANUAL_GROUPS.has(name))
    .sort();

console.log(`[coverage] 桥提供 ${provided.size} 个方法`);
console.log(`[coverage] 残留 electronAPI 访问 ${legacy.size} 个，缺失 ${legacyMissing.length} 个${legacyMissing.length ? ': ' + legacyMissing.join(', ') : ''}`);
console.log(`[coverage] oicppApi 访问 ${modern.size} 个，未实现 ${modernMissing.length} 个${modernMissing.length ? ': ' + modernMissing.join(', ') : ''}`);

if (legacyMissing.length || modernMissing.length) {
    process.exit(1);
}
