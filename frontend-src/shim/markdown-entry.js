/**
 * markdown / turndown 渲染逻辑（原 Electron 版本位于 preload.js，运行在 Node 侧）。
 * Tauri 版本没有 Node 环境，改为在渲染进程内打包执行，对外暴露同样的
 * `window.markdownAPI.render(text, filePath)` 与 `window.turndownAPI.toMarkdown(html)`。
 */

import MarkdownIt from 'markdown-it';
import mk from '@iktakahiro/markdown-it-katex';
import taskLists from 'markdown-it-task-lists';
import imageFigures from 'markdown-it-image-figures';
import hljs from 'highlight.js';
import TurndownService from 'turndown';

/** 与 preload.js 保持一致的数学公式归一化逻辑 */
function normalizeMarkdownMath(input) {
    if (!input || typeof input !== 'string') return input;

    const normalizeInlineMathInLine = (line) => {
        let out = '';
        let inCode = false;
        let codeFence = '';
        let i = 0;

        while (i < line.length) {
            const ch = line[i];
            if (ch === '`') {
                let count = 1;
                while (i + count < line.length && line[i + count] === '`') {
                    count++;
                }
                const fence = '`'.repeat(count);
                if (!inCode) {
                    inCode = true;
                    codeFence = fence;
                } else if (fence === codeFence) {
                    inCode = false;
                    codeFence = '';
                }
                out += fence;
                i += count;
                continue;
            }

            if (!inCode && ch === '$') {
                const next = line[i + 1];
                if (next === '$' || (i > 0 && line[i - 1] === '\\')) {
                    out += ch;
                    i += 1;
                    continue;
                }
                let j = i + 1;
                while (j < line.length) {
                    if (line[j] === '$' && line[j - 1] !== '\\') {
                        break;
                    }
                    j += 1;
                }
                if (j < line.length && line[j] === '$') {
                    const content = line.slice(i + 1, j);
                    const trimmed = content.replace(/^\s+|\s+$/g, '');
                    out += `$${trimmed.length ? trimmed : content}$`;
                    i = j + 1;
                    continue;
                }
            }

            out += ch;
            i += 1;
        }

        return out;
    };

    const lines = input.split('\n');
    let inFence = false;
    let fenceMarker = '';
    let inMathBlock = false;

    for (let idx = 0; idx < lines.length; idx++) {
        const line = lines[idx];
        const fenceMatch = line.match(/^\s{0,3}(```+|~~~+)/);
        if (fenceMatch) {
            const marker = fenceMatch[1][0];
            if (!inFence) {
                inFence = true;
                fenceMarker = marker;
            } else if (marker === fenceMarker) {
                inFence = false;
                fenceMarker = '';
            }
            continue;
        }
        if (!inFence) {
            const mathFenceMatch = line.match(/^\s*\$\$\s*$/);
            if (mathFenceMatch) {
                inMathBlock = !inMathBlock;
                lines[idx] = '$$';
                continue;
            }
            if (inMathBlock) {
                let normalized = line.trim();
                normalized = normalized
                    .replace(/\\begin\{align\*?\}/g, '\\begin{aligned}')
                    .replace(/\\end\{align\*?\}/g, '\\end{aligned}');
                lines[idx] = normalized;
                continue;
            }
            lines[idx] = normalizeInlineMathInLine(line);
        }
    }

    return lines.join('\n');
}

let md = null;
try {
    md = new MarkdownIt({
        html: true,
        linkify: true,
        typographer: true,
        highlight: function (str, lang) {
            if (lang && hljs.getLanguage(lang)) {
                try {
                    return '<pre class="hljs"><code>' +
                        hljs.highlight(str, { language: lang, ignoreIllegals: true }).value +
                        '</code></pre>';
                } catch (__) { }
            }
            return '<pre class="hljs"><code>' + md.utils.escapeHtml(str) + '</code></pre>';
        }
    })
        .use(mk, { throwOnError: false, strict: 'ignore' })
        .use(taskLists)
        .use(imageFigures, { figcaption: true });

    md.renderer.rules.fence = function (tokens, idx) {
        const token = tokens[idx];
        const code = token.content;
        const lang = token.info.trim();

        let highlighted;
        try {
            if (lang && hljs.getLanguage(lang)) {
                highlighted = '<pre class="hljs"><code>' +
                    hljs.highlight(code, { language: lang, ignoreIllegals: true }).value +
                    '</code></pre>';
            } else {
                highlighted = '<pre class="hljs"><code>' + hljs.highlightAuto(code).value + '</code></pre>';
            }
        } catch (__) {
            highlighted = '<pre class="hljs"><code>' + md.utils.escapeHtml(code) + '</code></pre>';
        }
        const encodedCode = encodeURIComponent(code);

        return `<div class="code-block-wrapper" style="position: relative;">
            <button class="copy-code-btn" type="button" data-code="${encodedCode}"
                    style="position: absolute; top: 5px; right: 5px; z-index: 10; padding: 4px 8px; background: rgba(255,255,255,0.1); border: 1px solid rgba(255,255,255,0.2); border-radius: 4px; color: inherit; cursor: pointer; font-size: 12px;">
                Copy
            </button>
            ${highlighted}
        </div>`;
    };
} catch (err) {
    console.error('Failed to initialize markdown-it:', err);
}

let turndownInstance = null;
try {
    turndownInstance = new TurndownService({
        headingStyle: 'atx',
        hr: '---',
        bulletListMarker: '-',
        codeBlockStyle: 'fenced',
        fence: '```',
        emDelimiter: '*',
        strongDelimiter: '**',
        linkStyle: 'inlined',
    });

    turndownInstance.addRule('taskListItem', {
        filter: (node) => node.nodeName === 'LI' && node.classList.contains('task-list-item'),
        replacement: (content, node) => {
            const checkbox = node.querySelector('input[type="checkbox"]');
            const checked = checkbox && checkbox.checked;
            return (checked ? '- [x] ' : '- [ ] ') + content.trim().replace(/^\[[ x]\]\s*/i, '') + '\n';
        }
    });

    turndownInstance.addRule('fencedCodeBlock', {
        filter: (node) => node.nodeName === 'PRE' && node.firstChild && node.firstChild.nodeName === 'CODE',
        replacement: (content, node, options) => {
            const code = node.firstChild;
            const langMatch = (code.getAttribute('class') || '').match(/language-(\S+)/);
            return '\n\n' + options.fence + (langMatch ? langMatch[1] : '') + '\n' + code.textContent + '\n' + options.fence + '\n\n';
        }
    });

    turndownInstance.addRule('hljsCodeBlock', {
        filter: (node) => node.nodeName === 'PRE' && node.classList.contains('hljs'),
        replacement: (content, node) => '\n\n```\n' + (node.textContent || '') + '\n```\n\n'
    });

    turndownInstance.addRule('ignoreCopyButton', {
        filter: (node) => node.nodeName === 'BUTTON' && node.classList.contains('copy-code-btn'),
        replacement: () => ''
    });

    turndownInstance.addRule('codeBlockWrapper', {
        filter: (node) => node.nodeName === 'DIV' && node.classList.contains('code-block-wrapper'),
        replacement: (content, node) => {
            const pre = node.querySelector('pre');
            if (pre) {
                return '\n\n```\n' + (pre.textContent || '') + '\n```\n\n';
            }
            return content;
        }
    });
} catch (e) {
    console.error('Failed to initialize Turndown:', e);
}

/** 与 preload.js 一致：把相对图片路径解析成可被 WebView 加载的 URL */
function resolveImageSrc(src, filePath) {
    if (!src || /^(https?:|data:|file:|tauri:|asset:|blob:)/i.test(src)) {
        return src;
    }
    if (!filePath) return src;
    const normalizedPath = String(filePath).replace(/\\/g, '/');
    const dir = normalizedPath.slice(0, normalizedPath.lastIndexOf('/'));
    let resolved = src.startsWith('/')
        ? src
        : (dir ? dir + '/' + src : src);
    const parts = [];
    for (const seg of resolved.split('/')) {
        if (!seg || seg === '.') continue;
        if (seg === '..') parts.pop();
        else parts.push(seg);
    }
    let joined = parts.join('/');
    if (!joined.startsWith('/')) joined = '/' + joined;
    const origin = window.__OICPP_ENV__ && window.__OICPP_ENV__.assetOrigin;
    if (origin) {
        return origin.replace(/\/$/, '') + joined;
    }
    return 'file://' + joined;
}

window.markdownAPI = {
    render: (text, filePath) => {
        if (!md) return text;
        try {
            const normalizedText = normalizeMarkdownMath(text || '');
            const defaultImageRender = md.renderer.rules.image || function (tokens, idx, options, env, self) {
                return self.renderToken(tokens, idx, options);
            };

            md.renderer.rules.image = function (tokens, idx, options, env, self) {
                const token = tokens[idx];
                const srcIndex = token.attrIndex('src');
                if (srcIndex >= 0) {
                    token.attrs[srcIndex][1] = resolveImageSrc(token.attrs[srcIndex][1], filePath);
                }
                return defaultImageRender(tokens, idx, options, env, self);
            };
            return md.render(normalizedText);
        } catch (err) {
            console.error('Markdown render error:', err);
            return text;
        }
    }
};

window.turndownAPI = {
    toMarkdown: (html) => {
        if (!turndownInstance) {
            const temp = document.createElement('div');
            temp.innerHTML = html;
            return temp.textContent || temp.innerText || '';
        }
        try {
            return turndownInstance.turndown(html);
        } catch (err) {
            console.error('Turndown error:', err);
            const temp = document.createElement('div');
            temp.innerHTML = html;
            return temp.textContent || temp.innerText || '';
        }
    }
};
