// ops-console Web UI 前端逻辑（无构建，原生 fetch + EventSource + hash 路由）
const $ = (id) => document.getElementById(id);
let TOKEN = localStorage.getItem('ops_token') || '';

async function api(path, opts = {}) {
  const headers = { 'Content-Type': 'application/json', ...(opts.headers || {}) };
  if (TOKEN) headers['Authorization'] = 'Bearer ' + TOKEN;
  const res = await fetch(path, { ...opts, headers });
  if (res.status === 401) { showLogin(); throw new Error('未授权'); }
  if (!res.ok) { const t = await res.text(); throw new Error(t || res.statusText); }
  return res.json();
}

// ---- 路由 ----
const pages = ['run', 'resources', 'config', 'history'];
function route() {
  const h = location.hash.replace('#/', '');
  const page = pages.includes(h) ? h : 'run';
  pages.forEach(p => $('page-' + p).classList.add('hidden'));
  $('page-' + page).classList.remove('hidden');
  document.querySelectorAll('[data-nav]').forEach(a =>
    a.classList.toggle('active', a.dataset.nav === page));
  if (page === 'resources') loadResources();
  if (page === 'config' && !cfgLoaded) loadConfig();
  if (page === 'history') loadHistory();
  if (page === 'run') refreshProjects();
}
window.addEventListener('hashchange', route);

// ---- 登录 ----
function showLogin() {
  $('page-login').classList.remove('hidden');
  $('loginState').textContent = '';
}
async function doLogin() {
  const token = $('loginToken').value.trim();
  try {
    const res = await fetch('/api/login', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ token }),
    });
    if (!res.ok) throw new Error('令牌错误');
    TOKEN = token; localStorage.setItem('ops_token', token);
    $('loginToken').value = '';
    $('page-login').classList.add('hidden');
    $('loginErr').textContent = '';
    $('loginState').textContent = '已登录';
    route();
  } catch (e) { $('loginErr').textContent = e.message; }
}
$('loginBtn').onclick = doLogin;
$('loginToken').addEventListener('keydown', e => { if (e.key === 'Enter') doLogin(); });

// ---- 运行命令页 ----
const CMD_PARAMS = {
  snapshot: [{ k: '--keep', label: '保留份数', def: '2', type: 'number' },
             { k: '--wait-minutes', label: '等待超时(分钟)', def: '30', type: 'number' }],
  expiry:   [{ k: '--days', label: '提醒阈值(天,逗号分隔)', def: '30,15,3', type: 'text' }],
  disk:     [{ k: '--threshold', label: '磁盘阈值(%)', def: '90', type: 'number' }],
};
function renderExtraParams() {
  const cmd = $('cmdSelect').value;
  const box = $('extraParams');
  box.innerHTML = '';
  (CMD_PARAMS[cmd] || []).forEach(p => {
    const wrap = document.createElement('div');
    wrap.className = 'mb-2';
    wrap.innerHTML = `<label class="form-label">${p.label}</label>
      <input type="${p.type}" class="form-control form-control-sm param-input" data-key="${p.k}" value="${p.def}">`;
    box.appendChild(wrap);
  });
}
$('cmdSelect').onchange = renderExtraParams;
renderExtraParams();

async function refreshProjects() {
  try {
    const cfg = await api('/api/config');
    window.__cfg = cfg;
    const sel = $('projectSelect');
    const cur = sel.value;
    sel.innerHTML = '<option value="">全部项目</option>' +
      cfg.projects.map(p => `<option value="${esc(p.name)}">${esc(p.name)}</option>`).join('');
    sel.value = cur;
    renderProviders(cfg, cur);
  } catch (e) { /* 未登录等情况由 api() 处理 */ }
}
$('projectSelect').onchange = () => {
  if (window.__cfg) renderProviders(window.__cfg, $('projectSelect').value);
};
function renderProviders(cfg, projectName) {
  const p = cfg.projects.find(x => x.name === projectName);
  $('providerSelect').innerHTML = '<option value="">全部</option>' +
    (p ? Object.keys(p.providers).map(k => `<option value="${esc(k)}">${esc(k)}</option>`).join('') : '');
}

let es = null;
// 当前提交的任务 id：status 事件按它过滤（排队场景下忽略前任务的终态）
let currentTaskId = null;
function startStream() {
  if (es) es.close();
  es = new EventSource(`/api/tasks/current/stream?token=${encodeURIComponent(TOKEN)}`);
  let esOpened = false;
  es.onopen = () => { esOpened = true; };
  es.addEventListener('line', e => appendOut(e.data));
  es.addEventListener('status', e => {
    // data 格式: "{task_id} {status}"（如 "abc123 success"）
    const sp = e.data.indexOf(' ');
    if (sp < 0) return;
    const id = e.data.slice(0, sp);
    const st = e.data.slice(sp + 1);
    // 非当前任务的终态（如排队时前任务的结束）：忽略，不更新徽标、不关流、不启用按钮
    if (id !== currentTaskId) return;
    const badge = $('runStatus');
    badge.textContent = st;
    badge.className = 'badge ' + (st === 'success' ? 'bg-success' : st === 'failed' ? 'bg-danger' : 'bg-primary');
    if (st === 'success' || st === 'failed') {
      es.close(); es = null;
      $('runBtn').disabled = false;
    }
  });
  es.onerror = () => {
    // 从未建立过连接即报错 → 大概率 401（token 失效/未登录）：跳登录页
    if (!esOpened) { es.close(); es = null; showLogin(); }
    // 已建立过连接后的断线由浏览器自动重连
  };
}

function appendOut(text) {
  const out = $('output');
  const cls = text.startsWith('ERR') || text.includes('失败') ? 'err' : text.includes('成功') || text.startsWith('✓') ? 'ok' : '';
  const div = document.createElement('div');
  if (cls) div.className = cls;
  div.textContent = text;
  out.appendChild(div);
  out.scrollTop = out.scrollHeight;
}

async function runTask() {
  const command = $('cmdSelect').value;
  const project = $('projectSelect').value || null;
  const provider = $('providerSelect').value || null;
  const extra = Array.from(document.querySelectorAll('.param-input'))
    .map(i => [i.dataset.key, i.value]).filter(([, v]) => v !== '').flat();
  $('output').innerHTML = '';
  $('runBtn').disabled = true;
  $('runStatus').textContent = '排队中';
  $('runStatus').className = 'badge bg-secondary';
  // 先提交拿到 task_id，再开流：排队场景下 status 事件按 currentTaskId 过滤
  try {
    const resp = await api('/api/run', { method: 'POST', body: JSON.stringify({ command, project, provider, extra }) });
    currentTaskId = resp.task_id;
    startStream();
  } catch (e) {
    currentTaskId = null;
    appendOut('提交失败: ' + e.message);
    es && es.close(); es = null;
    $('runBtn').disabled = false;
    $('runStatus').textContent = '失败';
    $('runStatus').className = 'badge bg-danger';
  }
}
$('runBtn').onclick = runTask;
$('clearOutBtn').onclick = () => $('output').innerHTML = '';

// ---- 资源列表页（左侧项目列表 + 右侧 provider Tab）----
let resProjects = [];        // 资源快照（/api/resources 的 projects）
let currentResProject = null; // 当前选中项目

async function loadResources() {
  loadSnapshot();
}

// 读内存快照（不查询阿里云，秒回）
async function loadSnapshot() {
  try {
    const data = await api('/api/resources');
    if (data.ready) {
      resProjects = data.projects || [];
      if (!currentResProject || !resProjects.find(p => p.name === currentResProject)) {
        currentResProject = resProjects.length ? resProjects[0].name : null;
      }
      renderResList();
      renderResDetail();
      return;
    }
    $('resProjectList').innerHTML = '<div class="text-muted small p-2">暂无数据</div>';
    if (data.loading) {
      $('resBody').innerHTML = '<div class="text-muted py-3">后台加载中（启动/配置变更后自动拉取，global 巡检需几秒）…</div>';
    } else {
      $('resBody').innerHTML = '<div class="alert alert-warning py-2">资源尚未加载' +
        (data.error ? `：${esc(data.error)}` : '') + '，点右上角“刷新”立即加载</div>';
    }
  } catch (e) {
    $('resBody').innerHTML = `<div class="alert alert-danger py-2">加载失败: ${esc(e.message)}</div>`;
  }
}

// 手动刷新：阻塞拉取阿里云并更新快照
async function refreshResources() {
  $('resBody').innerHTML = '<div class="text-muted py-3">正在刷新…（global 巡检需要几秒）</div>';
  try {
    const data = await api('/api/resources/refresh', { method: 'POST' });
    resProjects = data.projects || [];
    if (!currentResProject || !resProjects.find(p => p.name === currentResProject)) {
      currentResProject = resProjects.length ? resProjects[0].name : null;
    }
    renderResList();
    renderResDetail();
  } catch (e) {
    $('resBody').innerHTML = `<div class="alert alert-danger py-2">刷新失败: ${esc(e.message)}</div>`;
  }
}

function renderResList() {
  const items = resProjects.map(p => {
    const n = Object.keys(p.providers || {}).length;
    const badge = n ? `<span class="badge bg-secondary">${n} provider</span>` : '';
    // name 为主、描述为辅；无描述时直接用 name
    const label = p.description && p.description !== p.name
      ? `${p.name}（${p.description}）`
      : p.name;
    return `<button class="list-group-item list-group-item-action d-flex justify-content-between align-items-center res-proj ${p.name === currentResProject ? 'active' : ''}" data-name="${esc(p.name)}">${esc(label)}${badge}</button>`;
  }).join('');
  $('resProjectList').innerHTML = items || '<div class="text-muted small p-2">没有配置 aliyun 服务商（且填了凭据）的项目</div>';
  document.querySelectorAll('#resProjectList .res-proj').forEach(b =>
    b.onclick = () => { currentResProject = b.dataset.name; renderResList(); renderResDetail(); });
}

function renderResDetail() {
  const p = resProjects.find(x => x.name === currentResProject);
  if (!p) { $('resBody').innerHTML = '<div class="text-muted py-3">选择左侧项目</div>'; return; }
  const providers = p.providers || {};
  const kinds = Object.keys(providers);
  if (!kinds.length) {
    $('resBody').innerHTML = `<div class="card shadow-sm"><div class="card-body text-muted">项目 ${esc(p.name)} 未配置服务商（或未填凭据）</div></div>`;
    return;
  }
  // provider Tab（目前仅 aliyun，未来扩展 tencent/aws 自动出现）
  const tabs = kinds.map((k, i) =>
    `<button class="nav-link ${i === 0 ? 'active' : ''}" data-bs-toggle="tab" data-bs-target="#res-pane-${i}" type="button">${esc(k)}</button>`).join('');
  const panes = kinds.map((k, i) => `
    <div class="tab-pane fade ${i === 0 ? 'show active' : ''}" id="res-pane-${i}">
      ${renderGroup('SWAS 实例', providers[k].swas)}
      ${renderGroup('ECS 实例', providers[k].ecs)}
      ${renderGroup('域名', providers[k].domains)}
    </div>`).join('');
  $('resBody').innerHTML = `
    <div class="card shadow-sm">
      <div class="card-header fw-bold">${esc(p.name)}</div>
      <div class="card-body">
        <ul class="nav nav-tabs mb-2">${tabs}</ul>
        <div class="tab-content">${panes}</div>
      </div>
    </div>`;
}

function renderGroup(title, g) {
  if (!g.ok) {
    return `<div class="mt-2"><h6 class="d-inline-block me-2">${title}</h6>
      <span class="badge bg-danger">查询失败</span>
      <div class="alert alert-warning py-1 px-2 small">${esc(g.error || '未知错误')}</div></div>`;
  }
  if (!g.items.length) {
    const tip = g.error ? ` <span class="text-muted small">${esc(g.error)}</span>` : '';
    return `<div class="mt-2"><h6 class="d-inline-block me-2">${title}</h6>
      <span class="text-muted small">无资源</span>${tip}</div>`;
  }
  const rows = g.items.map(it => {
    const exp = it.expired_at ? fmtBjt(it.expired_at) : '—';
    const left = it.days_left == null ? '—' : dlBadge(it.days_left);
    const renew = it.auto_renew ? ' <span class="badge bg-info">自动续费</span>' : '';
    const status = it.status ? esc(it.status) : '—';
    return `<tr>
      <td>${esc(it.name)}${renew}</td>
      <td class="small text-muted">${esc(it.id)}</td>
      <td>${esc(it.region)}</td>
      <td>${status}</td>
      <td class="small">${exp}</td>
      <td>${left}</td>
    </tr>`;
  }).join('');
  return `<div class="mt-3"><h6 class="d-inline-block me-2">${title}</h6>
    <span class="badge bg-secondary">${g.items.length}</span>
    <div class="table-responsive"><table class="table table-sm table-hover align-middle mb-0">
      <thead><tr><th>名称</th><th>ID</th><th>地域</th><th>状态</th><th>到期时间（北京）</th><th>剩余</th></tr></thead>
      <tbody>${rows}</tbody></table></div></div>`;
}

function fmtBjt(iso) {
  const d = new Date(iso);
  const p = n => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}
function dlBadge(d) {
  const cls = d <= 0 ? 'bg-danger' : d <= 30 ? 'bg-warning text-dark' : 'bg-success';
  const label = d <= 0 ? `已过期 ${-d} 天` : `剩 ${d} 天`;
  return `<span class="badge ${cls}">${label}</span>`;
}

$('resRefreshBtn').onclick = refreshResources;

// ---- 项目配置页 ----
let cfgLoaded = false, currentProject = null, rawMode = false;

async function loadConfig() {
  cfgLoaded = true;
  try {
    const cfg = await api('/api/config');
    window.__cfg = cfg;
    renderProjectList(cfg.projects);
    renderNotify(cfg.notify);
    if (cfg.projects.length) selectProject(cfg.projects[0].name);
  } catch (e) { showCfgMsg(e.message, true); }
}

function renderProjectList(projects) {
  $('projectList').innerHTML = projects.map(p =>
    `<button class="list-group-item list-group-item-action proj-item" data-name="${esc(p.name)}">${esc(p.name)}</button>`
  ).join('');
  document.querySelectorAll('.proj-item').forEach(b =>
    b.onclick = () => selectProject(b.dataset.name));
}

function selectProject(name) {
  currentProject = name;
  const p = window.__cfg.projects.find(x => x.name === name);
  document.querySelectorAll('.proj-item').forEach(b =>
    b.classList.toggle('active', b.dataset.name === name));
  const ed = $('projectEditor');
  if (!p) { ed.innerHTML = '<p class="text-muted">选择一个项目</p>'; return; }
  let html = `
    <div class="mb-2"><label class="form-label">名称</label>
      <input class="form-control" id="projName" value="${esc(p.name)}"></div>
    <div class="mb-2"><label class="form-label">描述</label>
      <input class="form-control" id="projDesc" value="${esc(p.description || '')}"></div>
    <h6 class="mt-3">服务商</h6>`;
  Object.entries(p.providers || {}).forEach(([kind, pc]) => {
    html += `<div class="border rounded p-2 mb-2 provider-block" data-kind="${kind}"${pc._path ? ` data-path="${esc(pc._path)}"` : ''}>
      <div class="d-flex justify-content-between"><strong>${kind}</strong>
        <button class="btn btn-sm btn-outline-danger del-provider">删除</button></div>
      <div class="row g-2 mt-1">
        <div class="col-4"><label class="form-label small">region</label>
          <input class="form-control form-control-sm prov-region" value="${esc(pc.region || '')}"></div>
        <div class="col-4"><label class="form-label small">access_key_id</label>
          <input class="form-control form-control-sm prov-ak" value="${esc(pc.access_key_id || '')}"></div>
        <div class="col-4"><label class="form-label small">access_key_secret</label>
          <input type="password" class="form-control form-control-sm prov-sk" value="${esc(pc.access_key_secret || '')}"></div>
      </div></div>`;
  });
  html += `<button class="btn btn-sm btn-outline-primary mt-1" id="addProviderBtn">+ 添加服务商</button>
           <button class="btn btn-sm btn-outline-danger mt-1 ms-1" id="delProjectBtn">删除项目</button>`;
  ed.innerHTML = html;
  ed.querySelectorAll('.del-provider').forEach(b => b.onclick = () => {
    b.closest('.provider-block').remove();
  });
  $('delProjectBtn').onclick = () => {
    if (!confirm(`确认删除项目 ${p.name}？`)) return;
    window.__cfg.projects = window.__cfg.projects.filter(x => x.name !== p.name);
    renderProjectList(window.__cfg.projects);
    if (window.__cfg.projects.length) selectProject(window.__cfg.projects[0].name);
    else $('projectEditor').innerHTML = '<p class="text-muted">暂无项目，点击左上角新增</p>';
  };
  $('addProviderBtn').onclick = () => {
    // 内联下拉选择服务商 kind（列表来自后端 /api/providers，唯一权威来源）
    const list = window.__providers || [];
    if (!list.length) { alert('当前无可用的服务商'); return; }
    const holder = document.createElement('div');
    holder.className = 'mb-2 d-flex gap-2 align-items-center';
    holder.innerHTML = `
      <select class="form-select form-select-sm add-prov-select" style="max-width:220px">
        ${list.map(k => `<option value="${esc(k)}">${esc(k)}</option>`).join('')}
      </select>
      <button class="btn btn-sm btn-primary add-prov-ok">确定</button>
      <button class="btn btn-sm btn-outline-secondary add-prov-cancel">取消</button>`;
    $('addProviderBtn').insertAdjacentElement('beforebegin', holder);
    $('addProviderBtn').disabled = true;
    holder.querySelector('.add-prov-cancel').onclick = () => {
      holder.remove();
      $('addProviderBtn').disabled = false;
    };
    holder.querySelector('.add-prov-ok').onclick = () => {
      const kind = holder.querySelector('.add-prov-select').value;
      holder.remove();
      $('addProviderBtn').disabled = false;
      if (!kind) return;
      ed.insertBefore(createProviderBlock(kind), $('addProviderBtn').parentNode);
    };
  };
}

// 构建一个服务商编辑块（配置页）
function createProviderBlock(kind) {
  const wrap = document.createElement('div');
  wrap.className = 'border rounded p-2 mb-2 provider-block';
  wrap.dataset.kind = kind;
  wrap.innerHTML = `<div class="d-flex justify-content-between"><strong>${esc(kind)}</strong>
    <button class="btn btn-sm btn-outline-danger del-provider">删除</button></div>
    <div class="row g-2 mt-1">
      <div class="col-4"><input class="form-control form-control-sm prov-region" placeholder="region"></div>
      <div class="col-4"><input class="form-control form-control-sm prov-ak" placeholder="access_key_id"></div>
      <div class="col-4"><input type="password" class="form-control form-control-sm prov-sk" placeholder="access_key_secret"></div>
    </div>`;
  wrap.querySelector('.del-provider').onclick = () => wrap.remove();
  return wrap;
}

function collectProjects() {
  // 当前编辑的项目从表单收集（保留其 _path 与服务商 _path）；其余项目保留原值
  return window.__cfg.projects.map(p => {
    if (p.name !== currentProject) return p;
    const providers = {};
    document.querySelectorAll('#projectEditor .provider-block').forEach(block => {
      const kind = block.dataset.kind;
      providers[kind] = {
        region: block.querySelector('.prov-region').value,
        access_key_id: block.querySelector('.prov-ak').value,
        access_key_secret: block.querySelector('.prov-sk').value,
      };
      // 渲染时保留的 _path 原样回传；新增服务商无 _path
      if (block.dataset.path) providers[kind]._path = block.dataset.path;
    });
    const orig = window.__cfg.projects.find(x => x.name === currentProject) || {};
    return {
      name: $('projName').value.trim(),
      description: $('projDesc').value || null,
      providers,
      ...(orig._path ? { _path: orig._path } : {}),
    };
  });
}

function collectNotify() {
  return {
    kind: $('notifyKind').value,
    prefix: $('notifyPrefix').value,
    dingtalk: {
      webhook: $('notifyWebhook').value,
      secret: $('notifySecret').value,
    },
  };
}

function renderNotify(n) {
  $('notifyKind').value = n.kind || '';
  $('notifyPrefix').value = n.prefix || '';
  $('notifyWebhook').value = n.dingtalk?.webhook || '';
  $('notifySecret').value = n.dingtalk?.secret || '';
}

function showCfgMsg(text, isErr) {
  $('cfgMsg').innerHTML = `<div class="alert ${isErr ? 'alert-danger' : 'alert-success'} py-2">${esc(text)}</div>`;
  setTimeout(() => $('cfgMsg').innerHTML = '', 4000);
}
function esc(s) { return String(s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])); }

$('saveCfgBtn').onclick = async () => {
  try {
    if (rawMode) {
      const notify = $('rawNotify').value;
      await api('/api/config/raw', { method: 'POST', body: JSON.stringify({
        project_yml: $('rawProject').value, notify_yml: notify ? notify : null }) });
    } else {
      const projects = collectProjects();
      const notify = collectNotify();
      await api('/api/config', { method: 'POST', body: JSON.stringify({ projects, notify }) });
    }
    cfgLoaded = false; loadConfig();
    showCfgMsg('保存成功');
  } catch (e) { showCfgMsg(e.message, true); }
};

$('addProjectBtn').onclick = () => {
  const name = prompt('项目名称');
  if (!name) return;
  window.__cfg.projects.push({ name, description: null, providers: {} });
  renderProjectList(window.__cfg.projects);
  selectProject(name);
};

$('toggleRaw').onclick = async () => {
  rawMode = !rawMode;
  $('cfgForm').classList.toggle('hidden', rawMode);
  $('cfgRaw').classList.toggle('hidden', !rawMode);
  $('toggleRaw').textContent = rawMode ? '切换到表单' : '切换到 YAML 原文';
  if (rawMode) {
    const raw = await api('/api/config/raw');
    $('rawProject').value = raw.project_yml;
    $('rawNotify').value = raw.notify_yml || '';
  } else {
    // 切回表单：重新拉取配置，避免表单/缓存与磁盘不一致
    await refreshProjects();
  }
};

// ---- 历史页 ----
const STATUS_BADGE = { queued: 'bg-secondary', running: 'bg-primary', success: 'bg-success', failed: 'bg-danger' };
async function loadHistory() {
  try {
    const tasks = await api('/api/tasks');
    let curRow = '';
    try {
      const cur = await api('/api/tasks/current');
      if (cur && cur.status && (cur.status === 'running' || cur.status === 'queued')) {
        curRow = `<tr class="table-active">
          <td class="small">${esc((cur.submitted_at || '').replace('T', ' ').slice(0, 19))}</td>
          <td>${esc(cur.command)}</td>
          <td class="small text-muted">${esc((cur.args || []).slice(4).join(' '))}</td>
          <td><span class="badge ${STATUS_BADGE[cur.status] || 'bg-secondary'}">${cur.status}</span></td>
          <td class="small">执行中…</td>
          <td>—</td>
        </tr>`;
      }
    } catch (e) { /* 无当前任务 */ }
    $('historyBody').innerHTML = curRow + tasks.map(t => `
      <tr>
        <td class="small">${esc(t.submitted_at.replace('T', ' ').slice(0, 19))}</td>
        <td>${esc(t.command)}</td>
        <td class="small text-muted">${esc(t.args.slice(4).join(' '))}</td>
        <td><span class="badge ${STATUS_BADGE[t.status] || 'bg-secondary'}">${t.status}</span></td>
        <td class="small">${t.duration_secs != null ? t.duration_secs + 's' : '-'}</td>
        <td><button class="btn btn-sm btn-outline-secondary hist-view" data-id="${t.id}" data-cmd="${esc(t.command)}">查看输出</button></td>
      </tr>`).join('');
    document.querySelectorAll('.hist-view').forEach(b => b.onclick = async () => {
      const res = await fetch(`/api/tasks/${b.dataset.id}/output`, { headers: { Authorization: 'Bearer ' + TOKEN } });
      $('outModalTitle').textContent = `${b.dataset.cmd} · ${b.dataset.id.slice(0, 8)}`;
      $('outModalBody').textContent = await res.text();
      bootstrap.Modal.getOrCreateInstance($('outModal')).show();
    });
  } catch (e) { /* 忽略 */ }
}
setInterval(() => { if (!location.hash.includes('history')) return; loadHistory(); }, 5000);

// 从后端获取当前接入的服务商列表（供配置页“添加服务商”下拉使用）
async function loadProviders() {
  try {
    const d = await api('/api/providers');
    window.__providers = d.providers || [];
  } catch (e) { window.__providers = []; }
}

// ---- 启动 ----
if (!TOKEN) showLogin(); else { loadProviders(); route(); }
