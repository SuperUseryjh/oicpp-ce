/**
 * 迁移守门检查：渲染层对 `electronAPI` / `oicppApi` 的调用是否都有实现。
 *
 * 三项检查：
 *   1. 还残留的 `electronAPI.<方法>` 访问必须仍被桥覆盖（迁移期的兜底路径没被破坏）；
 *   2. 新的 `oicppApi.<方法>` 访问必须有来源 —— 要么在桥里（DIRECT / PASS_THROUGH），
 *      要么属于手工迁移的 API 分组。这条用于防止「改了个不存在的 API」这类静默失败。
 *   3. 每个页面里的 `<script src="...oicpp-api-legacy.js">` 都必须解析到生成器实际写出的文件。
 *      前两条都只读磁盘上的文件，桥写错目录时照样通过；但页面是按 URL 相对路径取脚本的，
 *      写错目录会 404 —— 表现为「整个应用炸掉 + i18n 全是键名」，且控制台只有一条资源 404。
 *
 * 用法：bun scripts/check-api-coverage.mjs
 *       退出码 1 表示有缺失（已接进 CI，会卡住构建）
 */
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const rendererDir = path.join(root, 'frontend-src', 'renderer');
const bridgePath = path.join(root, 'frontend', 'renderer', 'js', 'oicpp-api-legacy.js');

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

// 3) 页面引用路径必须解析到桥产物：frontend-src/renderer/X.html 会被原样复制到 frontend/renderer/X.html
const pageErrors = [];
for (const file of walk(rendererDir).filter((f) => f.endsWith('.html'))) {
    const html = fs.readFileSync(file, 'utf8');
    const rel = path.relative(path.join(root, 'frontend-src'), file); // renderer/index.html
    const pageDir = path.dirname(path.join(root, 'frontend', rel));
    for (const m of html.matchAll(/<script[^>]+src=["']([^"']*oicpp-api-legacy\.js)["']/g)) {
        const resolved = path.resolve(pageDir, m[1]);
        if (resolved !== bridgePath) {
            pageErrors.push(`${rel} 引用 ${m[1]} -> ${path.relative(root, resolved)}（期望 ${path.relative(root, bridgePath)}）`);
        }
    }
}
console.log(`[coverage] 页面引用桥 ${pageErrors.length ? '有误' : '一致'}${pageErrors.length ? ':\n  ' + pageErrors.join('\n  ') : ''}`);

if (legacyMissing.length || modernMissing.length || pageErrors.length) {
    process.exit(1);
}
