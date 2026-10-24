/**
 * BrowserManager - 内置浏览器管理器
 * 提供标签页内的网页浏览功能，支持分屏
 */

// 新建标签页默认显示的欢迎页（data URI，避免约:blank 一片空白）
const NEW_TAB_PAGE_HTML = (() => {
    const isDark = document.body.classList.contains('theme-light') ? false : true;
    const bg = isDark ? '#1e1e1e' : '#ffffff';
    const fg = isDark ? '#cccccc' : '#333333';
    const muted = isDark ? '#6a6a6a' : '#999999';
    const accent = '#007acc';
    // 构建极简欢迎页
    return `data:text/html;charset=utf-8,${encodeURIComponent(`
<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>新标签页</title>
<style>
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;
  background:${bg};color:${fg};display:flex;flex-direction:column;
  align-items:center;justify-content:center;height:100vh;user-select:none}
.logo{width:48px;height:48px;margin-bottom:16px;opacity:0.3;
  background:${accent};border-radius:12px;display:flex;align-items:center;justify-content:center}
.logo svg{width:28px;height:28px;fill:${bg}}
h1{font-size:18px;font-weight:400;margin-bottom:6px}
p{font-size:13px;color:${muted};margin-bottom:24px;text-align:center;line-height:1.6}
.hint{font-size:11px;color:${muted};opacity:0.6;margin-top:8px}
</style></head>
<body>
<div class="logo"><svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/><ellipse cx="12" cy="12" rx="4" ry="10"/><path d="M2 12h20"/></svg></div>
<h1>内置浏览器</h1>
<p>在上方地址栏输入网址开始浏览<br>支持 Ctrl+Shift+I 快速打开</p>
<div class="hint">支持标签页分屏 · 拖动标签到空白处即可</div>
</body></html>`)}`;
})();

class BrowserManager {
    constructor() {
        this.browserTabs = new Map(); // uniqueKey -> { container, frameWrapper, navBar, remoteId, ... }
        this._remoteToKey = new Map(); // Rust 侧 webview id -> uniqueKey
        this._remoteSeq = 0;
        this._currentFocusKey = null;
        this._newTabPage = NEW_TAB_PAGE_HTML;
        this._userAgent = '';

        if (window.oicppApi?.onBrowserOpenNewTab) {
            this._removeOpenNewTabListener = window.oicppApi.onBrowserOpenNewTab((request) => {
                this._handleOpenNewTabRequest(request);
            });
        }

        if (window.oicppApi?.onBrowserViewEvent) {
            this._removeViewEventListener = window.oicppApi.onBrowserViewEvent((payload) => {
                this._handleViewEvent(payload);
            });
        }

        // ResizeObserver 能覆盖尺寸变化；这里额外处理窗口缩放和滚动导致的坐标变化。
        this._layoutListener = () => {
            for (const [uniqueKey, state] of this.browserTabs.entries()) {
                if (state.remoteReady && state.container?.classList.contains('active')) {
                    this._scheduleSyncBounds(uniqueKey);
                }
            }
        };
        window.addEventListener('resize', this._layoutListener);
        document.addEventListener('scroll', this._layoutListener, true);
    }

    /**
     * 创建一个浏览器标签页容器
     * @param {object} options
     * @param {string} options.groupId - 所属编辑器分组 ID
     * @param {string} options.uniqueKey - 标签唯一键
     * @param {string} options.url - 初始 URL
     * @returns {HTMLElement} browserContainer
     */
    createBrowserContainer({ groupId, uniqueKey, url }) {
        // 未指定 URL 时使用内置新标签页
        const initialUrl = (url && url.trim()) ? url.trim() : this._newTabPage;
        // 主容器
        const container = document.createElement('div');
        container.className = 'browser-container';
        container.dataset.groupId = groupId || '';
        container.dataset.uniqueKey = uniqueKey || '';
        container.style.display = 'none';

        // 导航栏
        const navBar = this._createNavBar(uniqueKey, initialUrl);
        container.appendChild(navBar);

        // 远程 WebView 只覆盖这一层；导航栏仍由主 WebView 渲染
        const frameWrapper = document.createElement('div');
        frameWrapper.className = 'browser-webview-wrapper';
        frameWrapper.style.flex = '1';
        frameWrapper.style.position = 'relative';
        frameWrapper.style.overflow = 'hidden';

        // Rust 侧创建的子 WebView 无法嵌入 DOM，只能覆盖在主 WebView 上方。
        // 这里放一个占位层，负责显示加载/失败状态，真实页面由 browser-webview-* 命令控制。
        const remoteSurface = document.createElement('div');
        remoteSurface.className = 'browser-webview-surface';
        const placeholder = document.createElement('div');
        placeholder.className = 'browser-webview-placeholder';
        placeholder.textContent = window.i18n ? window.i18n.t('browser.loading') : '正在加载浏览器视图...';
        remoteSurface.appendChild(placeholder);
        frameWrapper.appendChild(remoteSurface);
        container.appendChild(frameWrapper);

        const initialHistory = this._isHistoryUrl(initialUrl) ? [initialUrl] : [];
        const state = {
            container,
            frameWrapper,
            remoteSurface,
            placeholder,
            navBar,
            urlInput: navBar.querySelector('.browser-url-input'),
            remoteId: '',
            remoteReady: false,
            remoteCreating: null,
            remoteError: '',
            destroyed: false,
            lastBounds: null,
            visible: false,
            title: '',
            isLoading: false,
            currentUrl: initialUrl,
            history: initialHistory,
            historyIndex: Math.max(0, initialHistory.length - 1),
            pendingHistoryMode: '',
            pendingHistoryUrl: '',
            syncRaf: 0,
            infoPollTimer: null,
            infoPollInFlight: false,
            loadingTimer: null,
            resizeObserver: null
        };
        this.browserTabs.set(uniqueKey, state);

        state.resizeObserver = new ResizeObserver(() => this._scheduleSyncBounds(uniqueKey));
        state.resizeObserver.observe(frameWrapper);

        this._updateNavButtons(uniqueKey);
        this._updateReloadButton(uniqueKey, false);
        return container;
    }

    /**
     * 创建导航栏
     */
    _createNavBar(uniqueKey, initialUrl) {
        const navBar = document.createElement('div');
        navBar.className = 'browser-nav-bar';

        // 导航按钮组
        const navButtons = document.createElement('div');
        navButtons.className = 'browser-nav-buttons';

        // 后退
        const backBtn = this._createNavButton('browser-nav-back', [
            '<svg width="16" height="16" viewBox="0 0 16 16">',
            '<path d="M10 3L5 8l5 5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>',
            '</svg>'
        ].join(''), '后退');
        navButtons.appendChild(backBtn);

        // 前进
        const forwardBtn = this._createNavButton('browser-nav-forward', [
            '<svg width="16" height="16" viewBox="0 0 16 16">',
            '<path d="M6 3l5 5-5 5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>',
            '</svg>'
        ].join(''), '前进');
        navButtons.appendChild(forwardBtn);

        // 刷新/停止
        const refreshBtn = this._createNavButton('browser-nav-reload', [
            '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">',
            '<polyline points="23 4 23 10 17 10"/>',
            '<path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10"/>',
            '</svg>'
        ].join(''), '刷新');
        refreshBtn.dataset.action = 'reload';
        navButtons.appendChild(refreshBtn);

        navBar.appendChild(navButtons);

        // URL 输入框
        const urlInput = document.createElement('input');
        urlInput.type = 'text';
        urlInput.className = 'browser-url-input';
        urlInput.setAttribute('placeholder', window.i18n ? window.i18n.t('browser.urlPlaceholder') : '输入网址或搜索...');
        urlInput.value = initialUrl?.startsWith('data:text/html') ? '' : (initialUrl || '');
        urlInput.setAttribute('spellcheck', 'false');
        urlInput.setAttribute('autocomplete', 'off');
        navBar.appendChild(urlInput);

        // 在新标签页中打开的按钮
        const openNewTabBtn = document.createElement('button');
        openNewTabBtn.className = 'browser-nav-action browser-nav-newtab';
        openNewTabBtn.innerHTML = [
            '<svg width="16" height="16" viewBox="0 0 16 16">',
            '<path d="M3 2h10a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1V3a1 1 0 0 1 1-1zm1 2v8h8V4H4z" fill="currentColor"/>',
            '<path d="M7 7V5h2v2h2v2H9v2H7V9H5V7h2z" fill="currentColor"/>',
            '</svg>'
        ].join('');
        openNewTabBtn.title = window.i18n ? window.i18n.t('browser.openInNewTab') : '新建浏览器标签页';
        navBar.appendChild(openNewTabBtn);

        // 导航按钮事件
        backBtn.addEventListener('click', () => this.goBack(uniqueKey));
        forwardBtn.addEventListener('click', () => this.goForward(uniqueKey));
        refreshBtn.addEventListener('click', () => {
            const state = this.browserTabs.get(uniqueKey);
            if (!state) return;
            if (state.isLoading) {
                this.stopLoading(uniqueKey);
            } else {
                this.reload(uniqueKey);
            }
        });
        urlInput.addEventListener('keydown', (e) => {
            if (e.key === 'Enter') {
                e.preventDefault();
                this.navigateTo(uniqueKey, urlInput.value);
            }
        });
        // 输入框获得焦点时选中全部文本
        urlInput.addEventListener('focus', () => {
            setTimeout(() => urlInput.select(), 0);
        });
        openNewTabBtn.addEventListener('click', () => {
            const state = this.browserTabs.get(uniqueKey);
            const url = state?.currentUrl || urlInput.value || 'about:blank';
            this._openInNewTab(url);
        });

        return navBar;
    }

    /**
     * 创建导航按钮
     */
    _createNavButton(className, innerHtml, title) {
        const btn = document.createElement('button');
        btn.className = 'browser-nav-btn ' + className;
        btn.innerHTML = innerHtml;
        btn.title = title || '';
        btn.tabIndex = -1;
        return btn;
    }

    /**
     * 处理 Rust 侧子 WebView 广播的事件
     */
    _handleViewEvent(payload) {
        if (!payload || typeof payload !== 'object') return;
        const uniqueKey = this._remoteToKey.get(payload.id || '');
        if (!uniqueKey) return;
        const state = this.browserTabs.get(uniqueKey);
        if (!state) return;

        const type = payload.type || '';
        if (type === 'did-start-loading') {
            this._startLoading(uniqueKey);
            return;
        }
        if (type === 'did-stop-loading') {
            const url = typeof payload.url === 'string' ? payload.url : '';
            if (url) {
                state.currentUrl = url;
                state.urlInput.value = this._displayUrl(url);
                this._rememberUrl(uniqueKey, url);
            }
            this._finishLoading(uniqueKey);
            return;
        }
        if (type === 'page-info') {
            this._applyPageInfo(uniqueKey, payload, true);
        }
    }

    _startLoading(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state) return;
        state.isLoading = true;
        this._updateReloadButton(uniqueKey, true);
        state.navBar.classList.add('loading');
        if (state.loadingTimer) clearTimeout(state.loadingTimer);
        // 某些失败页面不会触发 did-stop-loading，超时后恢复按钮状态
        state.loadingTimer = setTimeout(() => this._finishLoading(uniqueKey), 30000);
    }

    _finishLoading(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state) return;
        state.isLoading = false;
        if (state.loadingTimer) {
            clearTimeout(state.loadingTimer);
            state.loadingTimer = null;
        }
        this._updateReloadButton(uniqueKey, false);
        state.navBar.classList.remove('loading');
    }

    _applyPageInfo(uniqueKey, info, updateHistory) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state || !info) return;

        const url = typeof info.url === 'string' ? info.url : '';
        const rawTitle = typeof info.title === 'string' ? info.title : '';
        const title = rawTitle.replace(/\s+/g, ' ').trim().slice(0, 160);

        if (url) {
            state.currentUrl = url;
            state.urlInput.value = this._displayUrl(url);
            this._rememberUrl(uniqueKey, url);
            if (updateHistory) {
                this._updateHistoryFromInfo(state, url, info.navType || 'navigate');
            }
        }

        const displayTitle = title || this._titleFromUrl(url);
        if (displayTitle && displayTitle !== state.title) {
            state.title = displayTitle;
            this._updateTabTitle(uniqueKey, displayTitle);
        }

        this._updateNavButtons(uniqueKey);
    }

    _rememberUrl(uniqueKey, url) {
        const tabData = window.tabManager?.tabs?.get(uniqueKey);
        if (tabData) {
            tabData.browserUrl = url;
        }
    }

    _updateHistoryFromInfo(state, url, navType) {
        const mode = state.pendingHistoryMode;
        state.pendingHistoryMode = '';
        state.pendingHistoryUrl = '';

        if (!this._isHistoryUrl(url)) {
            return;
        }

        if (mode === 'navigate') {
            if (state.historyIndex >= 0 && state.historyIndex < state.history.length) {
                state.history[state.historyIndex] = url;
            } else {
                this._pushHistory(state, url);
            }
            return;
        }

        if (mode === 'back' || mode === 'forward') {
            const found = state.history.lastIndexOf(url);
            if (found >= 0) {
                state.historyIndex = found;
            } else if (state.historyIndex >= 0 && state.historyIndex < state.history.length) {
                state.history[state.historyIndex] = url;
            }
            return;
        }

        if (navType === 'reload') {
            return;
        }

        const current = state.history[state.historyIndex];
        if (url === current) {
            return;
        }

        const found = state.history.lastIndexOf(url);
        if (found >= 0 && (found === state.historyIndex - 1 || found === state.historyIndex + 1)) {
            state.historyIndex = found;
            return;
        }

        this._pushHistory(state, url);
    }

    _pushHistory(state, url) {
        if (!this._isHistoryUrl(url)) return;
        if (state.history[state.historyIndex] === url) return;
        if (state.historyIndex < state.history.length - 1) {
            state.history.splice(state.historyIndex + 1);
        }
        state.history.push(url);
        state.historyIndex = state.history.length - 1;
    }

    _isHistoryUrl(url) {
        return typeof url === 'string' && /^(https?|file):\/\//i.test(url);
    }

    _displayUrl(url) {
        if (!url || url === 'about:blank' || /^data:/i.test(url)) return '';
        return url;
    }

    _titleFromUrl(url) {
        if (!url) return '';
        try {
            const parsed = new URL(url);
            if (parsed.protocol === 'file:') {
                return decodeURIComponent(parsed.pathname.split('/').pop() || '');
            }
            return (parsed.hostname || '').replace(/^www\./i, '');
        } catch (_) {
            return '';
        }
    }

    _browserUserAgent() {
        if (this._userAgent) return this._userAgent;
        this._userAgent = (navigator.userAgent || '')
            .replace(/\sElectron\/\S+/ig, '')
            .replace(/\soicpp-ide\/\S+/ig, '')
            .replace(/\sOICPP[^\s]*/ig, '')
            .trim();
        return this._userAgent;
    }

    _boundsFor(state) {
        if (!state?.frameWrapper) return null;
        const rect = state.frameWrapper.getBoundingClientRect();
        const width = Math.round(rect.width);
        const height = Math.round(rect.height);
        if (width < 2 || height < 2) return null;
        return {
            x: Math.max(0, Math.round(rect.left)),
            y: Math.max(0, Math.round(rect.top)),
            width,
            height
        };
    }

    async _ensureRemoteView(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state || state.destroyed) return false;
        if (state.remoteReady) return true;
        if (state.remoteCreating) return state.remoteCreating;

        const bounds = this._boundsFor(state);
        if (!bounds) return false;

        const api = window.oicppApi;
        if (!api || typeof api.browserViewCreate !== 'function') {
            this._setPlaceholderError(state, '浏览器接口不可用');
            return false;
        }

        if (!state.remoteId) {
            state.remoteId = `browser-${Date.now().toString(36)}-${++this._remoteSeq}`;
            this._remoteToKey.set(state.remoteId, uniqueKey);
        }

        const url = state.currentUrl || this._newTabPage;
        state.remoteCreating = api.browserViewCreate({
            id: state.remoteId,
            url,
            x: bounds.x,
            y: bounds.y,
            width: bounds.width,
            height: bounds.height,
            userAgent: this._browserUserAgent()
        }).then((result) => {
            if (!result || result.ok === false) {
                throw new Error(result?.error || '创建浏览器视图失败');
            }
            state.remoteReady = true;
            state.remoteError = '';
            state.lastBounds = bounds;

            // 标签页可能在创建期间被关闭：此时必须立刻关闭刚创建的子 WebView。
            if (state.destroyed || this.browserTabs.get(uniqueKey) !== state) {
                try {
                    const pending = window.oicppApi?.browserViewClose?.({ id: state.remoteId });
                    if (pending && typeof pending.catch === 'function') pending.catch(() => {});
                } catch (_) {}
                return false;
            }

            // 子 WebView 创建后默认就是可见的，这里记录真实状态并立即按容器位置同步；
            // 若标签页在创建期间已经被隐藏，_syncBoundsNow 会把它重新隐藏。
            state.visible = true;
            if (state.placeholder) state.placeholder.style.display = 'none';
            this._syncBoundsNow(uniqueKey);
            return true;
        }).catch((error) => {
            const message = error?.message || String(error);
            logError('创建内置浏览器视图失败:', message);
            state.remoteError = message;
            this._setPlaceholderError(state, message);
            return false;
        }).finally(() => {
            state.remoteCreating = null;
        });

        return state.remoteCreating;
    }

    _setPlaceholderError(state, message) {
        if (!state?.placeholder) return;
        state.placeholder.style.display = '';
        state.placeholder.textContent = `浏览器视图创建失败：${message}`;
    }

    _scheduleSyncBounds(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state || state.syncRaf) return;
        state.syncRaf = requestAnimationFrame(() => {
            state.syncRaf = 0;
            this._syncBoundsNow(uniqueKey);
        });
    }

    _syncBoundsNow(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state || !state.remoteReady) return;

        const active = state.container.classList.contains('active')
            && state.container.style.display !== 'none';
        if (!active) {
            this._setRemoteVisible(state, false);
            return;
        }

        const bounds = this._boundsFor(state);
        if (!bounds) {
            this._setRemoteVisible(state, false);
            return;
        }

        this._setRemoteVisible(state, true);

        const last = state.lastBounds;
        if (last
            && last.x === bounds.x
            && last.y === bounds.y
            && last.width === bounds.width
            && last.height === bounds.height) {
            return;
        }
        state.lastBounds = bounds;

        const api = window.oicppApi;
        if (!api || typeof api.browserViewSetBounds !== 'function') return;
        api.browserViewSetBounds({
            id: state.remoteId,
            x: bounds.x,
            y: bounds.y,
            width: bounds.width,
            height: bounds.height
        }).catch((error) => {
            logWarn('同步内置浏览器尺寸失败:', error?.message || error);
        });
    }

    _setRemoteVisible(state, visible) {
        if (!state?.remoteReady || state.destroyed || state.visible === visible) return;
        const api = window.oicppApi;
        if (!api || typeof api.browserViewSetVisible !== 'function') return;
        const previous = state.visible;
        state.visible = visible;
        api.browserViewSetVisible({ id: state.remoteId, visible }).catch((error) => {
            state.visible = previous;
            logWarn('切换内置浏览器可见性失败:', error?.message || error);
        });
    }

    _startInfoPolling(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state || state.infoPollTimer) return;
        state.infoPollTimer = setInterval(() => {
            if (!state.remoteReady || state.isLoading) return;
            if (!state.container.classList.contains('active')) return;
            this._refreshPageInfo(uniqueKey);
        }, 2000);
    }

    _stopInfoPolling(state) {
        if (!state?.infoPollTimer) return;
        clearInterval(state.infoPollTimer);
        state.infoPollTimer = null;
    }

    _refreshPageInfo(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state?.remoteReady || state.infoPollInFlight) return;
        const api = window.oicppApi;
        if (!api || typeof api.browserViewState !== 'function') return;

        state.infoPollInFlight = true;
        api.browserViewState({ id: state.remoteId }).then((info) => {
            if (!info || typeof info !== 'object') return;
            if (this.browserTabs.get(uniqueKey) !== state) return;
            this._applyPageInfo(uniqueKey, info, false);
        }).catch((error) => {
            logWarn('读取内置浏览器状态失败:', error?.message || error);
        }).finally(() => {
            state.infoPollInFlight = false;
        });
    }

    _remoteAction(uniqueKey, action) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state?.remoteReady) return Promise.resolve();
        const api = window.oicppApi;
        if (!api || typeof api.browserViewAction !== 'function') return Promise.resolve();
        return api.browserViewAction({ id: state.remoteId, action }).catch((error) => {
            logWarn('内置浏览器操作失败:', action, error?.message || error);
        });
    }

    /**
     * 在新标签页中打开 URL
     */
    _openInNewTab(url, groupId) {
        if (!url || !window.tabManager) return;
        const resolvedUrl = url;
        // 使用 tabManager 打开新的浏览器标签
        window.tabManager.openBrowserTab({
            url: resolvedUrl,
            groupId: groupId || window.tabManager.activeGroupId
        });
    }

    /**
     * 处理网页中的 target="_blank"、window.open 和中键点击。
     * 主进程会拦截原生弹窗，并把目标地址送回这里创建 IDE 浏览器标签。
     */
    _handleOpenNewTabRequest(request) {
        const url = typeof request?.url === 'string' ? request.url : '';
        if (!url || !/^https?:\/\//i.test(url)) return;

        let sourceGroupId = '';
        const sourceRemoteId = typeof request?.sourceWebviewId === 'string'
            ? request.sourceWebviewId
            : '';
        if (sourceRemoteId) {
            const sourceKey = this._remoteToKey.get(sourceRemoteId);
            const sourceState = sourceKey ? this.browserTabs.get(sourceKey) : null;
            if (sourceState) {
                sourceGroupId = sourceState.container?.dataset?.groupId || '';
            }
        }

        this._openInNewTab(url, sourceGroupId);
    }

    /**
     * 导航到指定 URL
     */
    async navigateTo(uniqueKey, rawUrl) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state) return;
        let url = rawUrl ? rawUrl.trim() : '';
        if (!url) return;
        // 通过主进程解析 URL
        try {
            if (window.oicppApi && typeof window.oicppApi.browserResolveUrl === 'function') {
                url = await window.oicppApi.browserResolveUrl(url);
            }
        } catch (_) {}
        if (!url) return;

        const hadRemote = state.remoteReady;
        this._pushHistory(state, url);
        state.pendingHistoryMode = 'navigate';
        state.pendingHistoryUrl = url;
        state.currentUrl = url;
        state.urlInput.value = this._displayUrl(url);
        this._rememberUrl(uniqueKey, url);
        this._updateNavButtons(uniqueKey);

        if (!hadRemote) {
            this._startLoading(uniqueKey);
            const ready = await this._ensureRemoteView(uniqueKey);
            if (!ready) this._finishLoading(uniqueKey);
            return;
        }

        this._startLoading(uniqueKey);
        try {
            await window.oicppApi.browserViewNavigate({ id: state.remoteId, url });
        } catch (error) {
            state.pendingHistoryMode = '';
            state.pendingHistoryUrl = '';
            logWarn('内置浏览器导航失败:', error?.message || error);
            this._finishLoading(uniqueKey);
        }
    }

    /**
     * 后退
     */
    goBack(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state || !state.remoteReady || state.historyIndex <= 0) return;
        state.pendingHistoryMode = 'back';
        state.historyIndex -= 1;
        this._updateNavButtons(uniqueKey);
        this._startLoading(uniqueKey);
        this._remoteAction(uniqueKey, 'back');
    }

    /**
     * 前进
     */
    goForward(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state || !state.remoteReady || state.historyIndex >= state.history.length - 1) return;
        state.pendingHistoryMode = 'forward';
        state.historyIndex += 1;
        this._updateNavButtons(uniqueKey);
        this._startLoading(uniqueKey);
        this._remoteAction(uniqueKey, 'forward');
    }

    /**
     * 刷新
     */
    reload(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state || !state.remoteReady) return;
        this._startLoading(uniqueKey);
        this._remoteAction(uniqueKey, 'reload');
    }

    /**
     * 停止加载
     */
    stopLoading(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state || !state.remoteReady) return;
        this._finishLoading(uniqueKey);
        this._remoteAction(uniqueKey, 'stop');
    }

    /**
     * 更新导航按钮状态
     */
    _updateNavButtons(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state) return;
        const backBtn = state.navBar.querySelector('.browser-nav-back');
        const forwardBtn = state.navBar.querySelector('.browser-nav-forward');
        if (backBtn) backBtn.classList.toggle('disabled', state.historyIndex <= 0);
        if (forwardBtn) {
            forwardBtn.classList.toggle('disabled', state.historyIndex >= state.history.length - 1);
        }
    }

    /**
     * 更新刷新/停止按钮
     */
    _updateReloadButton(uniqueKey, isLoading) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state) return;
        const reloadBtn = state.navBar.querySelector('.browser-nav-reload');
        if (!reloadBtn) return;
        if (isLoading) {
            reloadBtn.innerHTML = [
                '<svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">',
                '<rect x="6" y="6" width="12" height="12" rx="2"/>',
                '</svg>'
            ].join('');
            reloadBtn.title = window.i18n ? window.i18n.t('browser.stop') : '停止';
            reloadBtn.dataset.action = 'stop';
        } else {
            reloadBtn.innerHTML = [
                '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">',
                '<polyline points="23 4 23 10 17 10"/>',
                '<path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10"/>',
                '</svg>'
            ].join('');
            reloadBtn.title = window.i18n ? window.i18n.t('browser.reload') : '刷新';
            reloadBtn.dataset.action = 'reload';
        }
    }

    /**
     * 更新标签标题
     */
    _updateTabTitle(uniqueKey, title) {
        if (!title || !window.tabManager) return;
        const tabData = window.tabManager.tabs.get(uniqueKey);
        if (!tabData) return;
        const displayTitle = title || tabData.fileName;
        tabData.fileName = displayTitle;
        if (tabData.element) {
            const label = tabData.element.querySelector('.tab-label');
            if (label) {
                label.textContent = displayTitle;
                label.title = displayTitle;
            }
        }
    }

    /**
     * 获取浏览器标签页的状态
     */
    getBrowserState(uniqueKey) {
        return this.browserTabs.get(uniqueKey) || null;
    }

    /**
     * 销毁浏览器标签页
     */
    destroyBrowserTab(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state) return;

        state.destroyed = true;
        this._stopInfoPolling(state);
        if (state.loadingTimer) {
            clearTimeout(state.loadingTimer);
            state.loadingTimer = null;
        }
        if (state.syncRaf) {
            cancelAnimationFrame(state.syncRaf);
            state.syncRaf = 0;
        }
        if (state.resizeObserver) {
            try { state.resizeObserver.disconnect(); } catch (_) {}
            state.resizeObserver = null;
        }

        if (state.remoteId) {
            this._remoteToKey.delete(state.remoteId);
            try {
                const pending = window.oicppApi?.browserViewClose?.({ id: state.remoteId });
                if (pending && typeof pending.catch === 'function') pending.catch(() => {});
            } catch (_) {}
        }

        try {
            if (state.container && state.container.parentNode) {
                state.container.parentNode.removeChild(state.container);
            }
        } catch (_) {}
        this.browserTabs.delete(uniqueKey);
        if (this._currentFocusKey === uniqueKey) {
            this._currentFocusKey = null;
        }
    }

    /**
     * 聚焦浏览器标签页
     */
    focusBrowserTab(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state) return;
        this._currentFocusKey = uniqueKey;
        if (state.urlInput) {
            setTimeout(() => state.urlInput.focus(), 100);
        }
    }

    /**
     * 获取或创建浏览器容器（供 TabManager 调用）
     */
    getOrCreateContainer({ groupId, uniqueKey, url }) {
        const existing = this.browserTabs.get(uniqueKey);
        if (existing && existing.container) {
            // 如果 groupId 变化，需要移动容器
            if (existing.container.dataset.groupId !== groupId) {
                existing.container.dataset.groupId = groupId;
            }
            this._scheduleSyncBounds(uniqueKey);
            return existing.container;
        }
        return this.createBrowserContainer({ groupId, uniqueKey, url });
    }

    /**
     * 显示浏览器容器（隐藏其他视图）
     */
    showBrowserContainer(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state || !state.container) return;
        state.container.classList.add('active');
        state.container.style.display = '';

        // 等浏览器容器完成布局后再计算子 WebView 的位置。
        requestAnimationFrame(async () => {
            if (this.browserTabs.get(uniqueKey) !== state) return;
            await this._ensureRemoteView(uniqueKey);
            this._syncBoundsNow(uniqueKey);
            if (state.remoteReady && state.container.classList.contains('active')) {
                this._startInfoPolling(uniqueKey);
                this._refreshPageInfo(uniqueKey);
            } else {
                this._stopInfoPolling(state);
            }
        });
    }

    /**
     * 隐藏浏览器容器
     */
    hideBrowserContainer(uniqueKey) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state || !state.container) return;
        state.container.classList.remove('active');
        state.container.style.display = 'none';
        this._setRemoteVisible(state, false);
        this._stopInfoPolling(state);
    }

    /**
     * 隐藏分组内所有浏览器容器
     */
    hideGroupBrowserContainers(groupId) {
        for (const [key, state] of this.browserTabs.entries()) {
            if (state.container && state.container.dataset.groupId === groupId) {
                state.container.classList.remove('active');
                state.container.style.display = 'none';
                this._setRemoteVisible(state, false);
                this._stopInfoPolling(state);
            }
        }
    }

    /**
     * 移动浏览器容器到新分组（直接搬移 DOM，不重建子 WebView）
     */
    moveContainerToGroup(uniqueKey, newGroupId) {
        const state = this.browserTabs.get(uniqueKey);
        if (!state || !state.container) return null;

        // 只更新分组 ID，DOM 节点由调用方通过 appendChild 搬移
        state.container.dataset.groupId = newGroupId;
        state.lastBounds = null;
        this._scheduleSyncBounds(uniqueKey);

        return state.container;
    }
}

// 全局实例
window.browserManager = new BrowserManager();
