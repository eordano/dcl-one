#!/usr/bin/env python3
"""Render the explorable drift site from the collected data, the research and the review.

Inputs (all optional but the data):
  .cache/upstream-drift/data.json                  tooling/upstream-drift.py collect
  rendering/bevy-explorer/drift/research/*.json    per-source classification (features, performance,
                                                   security, others) written by the research agents
  rendering/bevy-explorer/drift/REVIEW.md          the summary and recommendations
  rendering/bevy-explorer/drift/recommendations.json  {url: {verdict, priority, why}}
Output: rendering/bevy-explorer/drift/index.html, one self-contained page (data embedded) that opens
from a file:// url or any static host, and .cache/upstream-drift/drift-site.tar.gz with the page, the
research and the review. The drift directory is git-ignored: the map is a build artifact.
"""
import html
import json
from pathlib import Path
import re
import sys
import tarfile

ROOT = Path(__file__).resolve().parents[1]
CACHE = ROOT / '.cache/upstream-drift'
OUT = ROOT / 'rendering/bevy-explorer/drift'


def slug(url):
    return re.sub(r'[^a-z0-9]+', '-', url.lower().removeprefix('https://github.com/')).strip('-')


def markdown_to_html(text):
    """Enough markdown for the review: headings, lists, tables, paragraphs, links, code, emphasis."""
    def inline(s):
        s = html.escape(s)
        s = re.sub(r'`([^`]+)`', r'<code>\1</code>', s)
        s = re.sub(r'\*\*([^*]+)\*\*', r'<strong>\1</strong>', s)
        s = re.sub(r'\[([^\]]+)\]\(([^)]+)\)', r'<a href="\2" target="_blank" rel="noreferrer">\1</a>', s)
        return s
    out, para, list_tag, table = [], [], None, []
    def flush_para():
        nonlocal para
        if para:
            out.append('<p>' + inline(' '.join(para)) + '</p>'); para = []
    def flush_list():
        nonlocal list_tag
        if list_tag:
            out.append(f'</{list_tag}>'); list_tag = None
    def flush_table():
        nonlocal table
        if table:
            rows = [r for r in table if not re.match(r'^\|?\s*:?-{2,}', r)]
            cells = [[inline(c.strip()) for c in r.strip().strip('|').split('|')] for r in rows]
            if cells:
                out.append('<table><thead><tr>' + ''.join(f'<th>{c}</th>' for c in cells[0]) + '</tr></thead><tbody>' +
                           ''.join('<tr>' + ''.join(f'<td>{c}</td>' for c in r) + '</tr>' for r in cells[1:]) + '</tbody></table>')
            table = []
    for line in text.splitlines():
        if line.startswith('|'):
            flush_para(); flush_list(); table.append(line); continue
        flush_table()
        m = re.match(r'^(#{1,4})\s+(.*)', line)
        if m:
            flush_para(); flush_list()
            level = len(m.group(1)); title = m.group(2).strip()
            out.append(f'<h{level} id="{slug(title)}">{inline(title)}</h{level}>'); continue
        m = re.match(r'^\s*([-*]|\d+\.)\s+(.*)', line)
        if m:
            flush_para()
            tag = 'ol' if m.group(1)[0].isdigit() else 'ul'
            if list_tag != tag:
                flush_list(); out.append(f'<{tag}>'); list_tag = tag
            out.append(f'<li>{inline(m.group(2))}</li>'); continue
        if not line.strip():
            flush_para(); flush_list(); continue
        flush_list(); para.append(line.strip())
    flush_para(); flush_list(); flush_table()
    return '\n'.join(out)


PAGE = r'''<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Explorer dependency drift</title>
<style>
:root{--bg:#0f1117;--panel:#171a23;--line:#2a2f3d;--fg:#e6e8ef;--muted:#9aa3b5;--acc:#7c9cff;--feat:#4fb36b;--perf:#e0a93a;--sec:#e5534b;--oth:#8b93a7;--upg:#e5534b;--keep:#4fb36b;--watch:#e0a93a;--drop:#7c9cff}
*{box-sizing:border-box}body{margin:0;font:14px/1.45 system-ui,-apple-system,Segoe UI,sans-serif;background:var(--bg);color:var(--fg)}
a{color:var(--acc);text-decoration:none}a:hover{text-decoration:underline}
header{display:flex;gap:16px;align-items:center;padding:10px 16px;border-bottom:1px solid var(--line);background:var(--panel);position:sticky;top:0;z-index:2}
header h1{font-size:16px;margin:0;font-weight:600}header nav a{margin-right:12px;color:var(--muted)}header nav a.on{color:var(--fg);border-bottom:2px solid var(--acc)}
header input{margin-left:auto;background:var(--bg);border:1px solid var(--line);color:var(--fg);padding:6px 10px;border-radius:6px;width:260px}
main{display:grid;grid-template-columns:300px 1fr;min-height:calc(100vh - 45px)}
aside{border-right:1px solid var(--line);overflow:auto;max-height:calc(100vh - 45px);position:sticky;top:45px}
aside .item{padding:9px 14px;border-bottom:1px solid var(--line);cursor:pointer;display:flex;flex-direction:column;gap:3px}aside .item:hover,aside .item.on{background:var(--panel)}
aside .item .n{font-weight:600}aside .item .m{color:var(--muted);font-size:12px}
section{padding:18px 24px;max-width:1200px}h2{margin:0 0 8px;font-size:20px}h3{margin:22px 0 8px;font-size:15px;color:var(--muted);text-transform:uppercase;letter-spacing:.04em}
.badge{display:inline-block;padding:1px 8px;border-radius:999px;font-size:11px;font-weight:600;border:1px solid var(--line);color:var(--muted);margin-right:6px;vertical-align:middle}
.b-features{border-color:var(--feat);color:var(--feat)}.b-performance{border-color:var(--perf);color:var(--perf)}.b-security{border-color:var(--sec);color:var(--sec)}.b-others{border-color:var(--oth);color:var(--oth)}
.b-upgrade{border-color:var(--upg);color:var(--upg)}.b-keep{border-color:var(--keep);color:var(--keep)}.b-watch{border-color:var(--watch);color:var(--watch)}.b-drop,.b-replace{border-color:var(--drop);color:var(--drop)}
.metrics{display:grid;grid-template-columns:repeat(auto-fill,minmax(150px,1fr));gap:10px;margin:12px 0}
.metric{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:10px 12px}.metric .v{font-size:20px;font-weight:600}.metric .l{color:var(--muted);font-size:12px}
table{border-collapse:collapse;width:100%;margin:8px 0 16px}th,td{border-bottom:1px solid var(--line);padding:6px 8px;text-align:left;vertical-align:top}th{color:var(--muted);font-weight:600;font-size:12px;cursor:pointer}
tr.aff td{background:rgba(229,83,75,.08)}.muted{color:var(--muted)}.tabs{display:flex;gap:6px;margin:14px 0 8px;flex-wrap:wrap}.tabs button{background:var(--panel);color:var(--muted);border:1px solid var(--line);border-radius:6px;padding:6px 12px;cursor:pointer}.tabs button.on{color:var(--fg);border-color:var(--acc)}
.cat{margin:10px 0 18px}.cat h4{margin:0 0 6px;font-size:13px}.cat li{margin:4px 0}.cat .ref{color:var(--muted);font-size:12px;margin-left:6px}
details{border:1px solid var(--line);border-radius:8px;padding:8px 12px;margin:8px 0;background:var(--panel)}summary{cursor:pointer;font-weight:600}pre{white-space:pre-wrap;color:var(--muted);font-size:12px;max-height:420px;overflow:auto}
.filters{display:flex;gap:8px;flex-wrap:wrap;margin:8px 0}.filters label{color:var(--muted)}.filters select,.filters input{background:var(--panel);color:var(--fg);border:1px solid var(--line);border-radius:6px;padding:4px 8px}
.review{max-width:900px}.review h1{font-size:22px}.review h2{margin-top:26px}.review code{background:var(--panel);padding:1px 5px;border-radius:4px}
.empty{color:var(--muted);font-style:italic}.sum{background:var(--panel);border-left:3px solid var(--acc);padding:10px 14px;border-radius:6px;margin:10px 0}
</style>
</head>
<body>
<header><h1>Explorer dependency drift</h1><nav id="nav"></nav><input id="q" placeholder="filter sources, crates, advisories…"></header>
<main><aside id="side"></aside><section id="view"></section></main>
<script id="data" type="application/json">__DATA__</script>
<script>
const D = JSON.parse(document.getElementById('data').textContent);
const R = D.research || {}, REC = D.recommendations || {};
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const name = s => (s.repo || s.url).replace(/^https:\/\/github\.com\//, '');
const crates = s => s.crates || [];
const CATS = ['features','performance','security','others'];
const state = { view: location.hash.replace('#','').split('/')[0] || 'overview', sel: decodeURIComponent(location.hash.split('/')[1] || ''), tab: 'missing', q: '' };
const nav = [['overview','Overview'],['sources','Sources'],['advisories','Advisories'],['dependencies','Dependencies'],['review','Review']];
function go(view, sel){ state.view = view; if (sel !== undefined) state.sel = sel; location.hash = view + (state.sel ? '/' + encodeURIComponent(state.sel) : ''); render(); }
window.addEventListener('hashchange', () => { const [v, s] = location.hash.replace('#','').split('/'); state.view = v || 'overview'; state.sel = decodeURIComponent(s || ''); render(); });
document.getElementById('q').addEventListener('input', e => { state.q = e.target.value.toLowerCase(); render(); });
const rec = s => REC[s.url] || null;
const n = x => x == null ? '–' : x.toLocaleString();
const carried = s => s.carried ? s.carried.commits.length : null;
const missing = s => s.upstream_since_pin ? s.upstream_since_pin.total_commits : null;
const watched = s => s.branch ? (s.base ? 'tag ' : 'branch ') + s.branch : 'pinned revision';
const affecting = s => { const names = new Set(crates(s).map(c => c.name)); return (D.advisories.findings||[]).filter(f => names.has(f.crate) && f.affected).length; };
const matches = s => !state.q || [name(s), s.name, s.branch, s.reason, ...crates(s).map(c => c.name)].join(' ').toLowerCase().includes(state.q);
function renderNav(){ document.getElementById('nav').innerHTML = nav.map(([k,l]) => `<a href="#${k}" class="${state.view===k?'on':''}">${l}</a>`).join(''); }
function renderSide(){
  const side = document.getElementById('side');
  if (state.view !== 'sources') { side.innerHTML = ''; side.style.display='none'; document.querySelector('main').style.gridTemplateColumns='1fr'; return; }
  side.style.display=''; document.querySelector('main').style.gridTemplateColumns='300px 1fr';
  side.innerHTML = D.sources.filter(matches).map(s => { const r = rec(s); return `<div class="item ${state.sel===s.url?'on':''}" onclick="go('sources', ${JSON.stringify(s.url)})">
    <div class="n">${esc(name(s))}</div><div class="m">${esc(s.carried ? 'patched · ' + watched(s) : watched(s))}</div>
    <div>${r ? `<span class="badge b-${esc(r.verdict)}">${esc(r.verdict)}</span>` : ''}${s.carried ? `<span class="badge">${n(carried(s))} patch(es)</span>` : ''}<span class="badge">${n(missing(s))} missing</span>${affecting(s) ? `<span class="badge b-security">${affecting(s)} advisories</span>` : ''}</div></div>`; }).join('');
}
function overview(){
  const rows = D.sources.filter(matches);
  const tot = { carried: 0, missing: 0, adv: 0 }; rows.forEach(s => { tot.carried += carried(s)||0; tot.missing += missing(s)||0; tot.adv += affecting(s); });
  const verdicts = {}; rows.forEach(s => { const r = rec(s); if (r) verdicts[r.verdict] = (verdicts[r.verdict]||0)+1; });
  return `<h2>Explorer at <code>${esc(D.explorer.revision.slice(0,12))}</code></h2>
  <p class="muted">Generated ${esc(D.generated)}. ${D.lock.packages} packages in the lock: ${Object.entries(D.lock.by_source).map(([k,v])=>`${v} ${k}`).join(', ')}; ${D.lock.duplicates.length} crates present in more than one version.</p>
  <div class="metrics"><div class="metric"><div class="v">${rows.length}</div><div class="l">pinned sources</div></div><div class="metric"><div class="v">${rows.filter(s=>s.carried).length}</div><div class="l">patched sources</div></div>
  <div class="metric"><div class="v">${n(tot.carried)}</div><div class="l">patches carried</div></div><div class="metric"><div class="v">${n(tot.missing)}</div><div class="l">upstream commits past the pins</div></div>
  <div class="metric"><div class="v">${(D.advisories.findings||[]).filter(f=>f.affected).length}</div><div class="l">advisories affecting the lock</div></div>
  ${Object.entries(verdicts).map(([k,v])=>`<div class="metric"><div class="v">${v}</div><div class="l">${esc(k)}</div></div>`).join('')}</div>
  ${D.review_summary ? `<div class="sum">${D.review_summary}</div>` : ''}
  <h3>Sources</h3><table><thead><tr><th>source</th><th>pinned tag</th><th>latest release</th><th>patches</th><th>missing</th><th>releases missed</th><th>newer tags</th><th>ref moved</th><th>advisories</th><th>verdict</th></tr></thead><tbody>
  ${rows.map(s => { const r = rec(s); return `<tr><td><a href="#sources/${encodeURIComponent(s.url)}">${esc(name(s))}</a><br><span class="muted">${esc(crates(s).slice(0,3).map(c=>c.name).join(', '))}${crates(s).length>3?' +'+(crates(s).length-3):''}</span></td>
    <td>${esc(s.base?.tag||'–')}<br><span class="muted">${esc(s.base?.date||'')}</span></td><td>${esc(s.parent_latest_release?.tag||'–')}<br><span class="muted">${esc(s.parent_latest_release?.date||'')}</span></td>
    <td>${n(carried(s))}</td><td>${n(missing(s))}</td><td>${s.releases_since_base ? s.releases_since_base.filter(r=>!r.prerelease).length : '–'}</td><td>${s.newer_tags ? s.newer_tags.length : '–'}</td><td>${s.branch_since_pin ? s.branch_since_pin.total_commits : '–'}</td>
    <td>${affecting(s) ? `<span class="badge b-security">${affecting(s)}</span>` : '0'}</td><td>${r ? `<span class="badge b-${esc(r.verdict)}">${esc(r.verdict)}</span><span class="muted">${esc(r.priority||'')}</span>` : '<span class="muted">–</span>'}</td></tr>`; }).join('')}</tbody></table>`;
}
function catList(obj){
  if (!obj) return '<p class="empty">Not researched.</p>';
  return CATS.map(c => { const items = obj[c] || []; return `<div class="cat"><h4><span class="badge b-${c}">${c}</span> ${items.length}</h4>${items.length ? '<ul>' + items.map(i => `<li>${esc(i.text || i)}${i.ref ? (/^https?:/.test(i.ref) ? `<a class="ref" href="${esc(i.ref)}" target="_blank" rel="noreferrer">${esc(i.ref.replace(/^https?:\/\/(www\.)?/, '').slice(0, 60))}</a>` : `<span class="ref">${esc(i.ref)}</span>`) : ''}</li>`).join('') + '</ul>' : '<p class="empty">none identified</p>'}</div>`; }).join('');
}
function commitTable(cmp, repo){
  if (!cmp) return '<p class="empty">no data</p>';
  const list = cmp.commits || [];
  return `<p class="muted">${cmp.total_commits} commit(s)${list.length < cmp.total_commits ? `, ${list.length} listed` : ''}</p><table><thead><tr><th>date</th><th>commit</th><th>author</th><th>title</th></tr></thead><tbody>${list.map(c => `<tr><td>${esc(c.date)}</td><td>${c.local ? `<span class="muted" title="${esc(c.file)}">local patch</span>` : `<a href="${esc(c.url || `https://github.com/${repo}/commit/${c.sha}`)}" target="_blank" rel="noreferrer"><code>${esc(c.sha.slice(0,8))}</code></a>`}</td><td>${esc(c.author)}</td><td>${esc(c.title)}</td></tr>`).join('')}</tbody></table>`;
}
function source(){
  const s = D.sources.find(x => x.url === state.sel) || D.sources[0]; if (!s) return '<p class="empty">no sources</p>'; state.sel = s.url;
  const r = rec(s), res = R[s.url];
  const tabs = [['missing','Missing from upstream'],['carried','Patches carried'],['releases','Releases missed'],['commits','Commit lists'],['advisories','Advisories'],['recommendation','Recommendation']];
  let body = '';
  if (state.tab === 'missing') body = (res ? `<div class="sum">${esc(res.summary||'')}</div>` : '') + catList(res && res.missing) + (res && res.upstreamed && res.upstreamed.length ? `<h3>Carried changes already upstream</h3><ul>${res.upstreamed.map(i=>`<li>${esc(i.text||i)}${i.ref?` <span class="ref">${esc(i.ref)}</span>`:''}</li>`).join('')}</ul>` : '');
  else if (state.tab === 'carried') body = catList(res && res.carried) + '<h3>The patch series</h3>' + (s.carried ? commitTable({total_commits: carried(s), commits: s.carried.commits}, s.repo) : '<p class="empty">no patches: the lock takes this source as upstream wrote it</p>');
  else if (state.tab === 'releases') body = (s.releases_since_base||[]).length ? s.releases_since_base.slice().reverse().map(rel => `<details><summary>${esc(rel.tag)} <span class="muted">${esc(rel.date)}${rel.prerelease?' · pre-release':''}</span> <a href="${esc(rel.url)}" target="_blank" rel="noreferrer">↗</a></summary><pre>${esc(rel.body||'(no notes)')}</pre></details>`).join('') : '<p class="empty">no releases after the pinned tag, or the source names no tag</p>';
  else if (state.tab === 'commits') body = `<h3>The default branch since the pin</h3>${commitTable(s.upstream_since_pin, s.repo)}<h3>The watched ref since the pin</h3>${commitTable(s.branch_since_pin, s.repo)}`;
  else if (state.tab === 'advisories') { const names = new Set(crates(s).map(c=>c.name)); const f = (D.advisories.findings||[]).filter(x=>names.has(x.crate)); body = f.length ? advisoryTable(f) : '<p class="empty">no RustSec entries for these crates</p>'; }
  else body = r ? `<p><span class="badge b-${esc(r.verdict)}">${esc(r.verdict)}</span> <span class="badge">${esc(r.priority||'')} priority</span></p><div class="sum">${esc(r.why||'')}</div>${r.how ? `<h3>How</h3><p>${esc(r.how)}</p>` : ''}` : '<p class="empty">No recommendation written.</p>';
  return `<h2>${esc(name(s))} ${r ? `<span class="badge b-${esc(r.verdict)}">${esc(r.verdict)}</span>` : ''}</h2>
  <p class="muted">${esc(s.description || '')}</p>
  <p><a href="${esc(s.url)}/tree/${esc(s.rev)}" target="_blank" rel="noreferrer">${esc(s.branch ? s.branch + ' @ ' : '')}${esc(s.rev.slice(0,12))}</a> (${esc(s.rev_date||'')})${crates(s).length ? ' · crates: ' + crates(s).map(c=>`<code>${esc(c.name)} ${esc(c.version)}</code>`).join(' ') : ''}</p>
  ${s.reason ? `<div class="sum"><strong>Upstream's reason:</strong> ${esc(s.reason)}</div>` : ''}
  <div class="metrics"><div class="metric"><div class="v">${esc(s.base?.tag||'–')}</div><div class="l">pinned tag ${esc(s.base?.date||'')}</div></div><div class="metric"><div class="v">${esc(s.parent_latest_release?.tag||'–')}</div><div class="l">latest upstream release ${esc(s.parent_latest_release?.date||'')}</div></div>
  <div class="metric"><div class="v">${n(carried(s))}</div><div class="l">patches carried</div></div><div class="metric"><div class="v">${n(missing(s))}</div><div class="l">upstream commits missing</div></div>
  <div class="metric"><div class="v">${s.releases_since_base ? s.releases_since_base.filter(x=>!x.prerelease).length : '–'}</div><div class="l">releases missed</div></div><div class="metric"><div class="v">${s.branch_since_pin ? s.branch_since_pin.total_commits : '–'}</div><div class="l">commits on the watched ref since the pin</div></div>
  <div class="metric"><div class="v">${s.newer_tags ? s.newer_tags.length : '–'}</div><div class="l">newer tags${s.newer_tags && s.newer_tags.length ? ', newest ' + esc(s.newer_tags[s.newer_tags.length-1]) : ''}</div></div><div class="metric"><div class="v">${affecting(s)}</div><div class="l">advisories affecting</div></div></div>
  <div class="tabs">${tabs.map(([k,l]) => `<button class="${state.tab===k?'on':''}" onclick="state.tab='${k}';render()">${l}</button>`).join('')}</div>${body}`;
}
function advisoryTable(f){
  return `<table><thead><tr><th>crate</th><th>locked</th><th>advisory</th><th>title</th><th>date</th><th>patched</th><th>affected</th></tr></thead><tbody>${f.map(x => `<tr class="${x.affected?'aff':''}"><td>${esc(x.crate)}</td><td>${esc(x.version)}</td><td><a href="${esc(x.rustsec_url)}" target="_blank" rel="noreferrer">${esc(x.id)}</a>${x.informational?`<br><span class="muted">${esc(x.informational)}</span>`:''}</td><td>${esc(x.title)}</td><td>${esc(x.date)}</td><td>${esc((x.patched||[]).join(', ')||'–')}</td><td>${x.affected?'<span class="badge b-security">yes</span>':'no'}</td></tr>`).join('')}</tbody></table>`;
}
function advisories(){
  const all = D.advisories.findings || []; const q = state.q;
  const f = all.filter(x => !q || [x.crate, x.id, x.title, ...(x.aliases||[])].join(' ').toLowerCase().includes(q));
  return `<h2>RustSec advisories</h2><p class="muted">${D.advisories.crates_with_entries.length} crates in the lock have advisory-db entries; ${all.filter(x=>x.affected).length} entries affect the pinned versions (informational entries such as unmaintained notices included).</p>${advisoryTable(f.sort((a,b)=>(b.affected-a.affected)||(b.date>a.date?1:-1)))}`;
}
let depSort = 'gap';
function dependencies(){
  const order = {major:0,minor:1,patch:2,current:3};
  const rows = D.registry.filter(r => !state.q || [r.name, r.description].join(' ').toLowerCase().includes(state.q)).sort((a,b)=> depSort==='gap' ? order[a.gap]-order[b.gap] || a.name.localeCompare(b.name) : a.name.localeCompare(b.name));
  const counts = {}; D.registry.forEach(r => counts[r.gap] = (counts[r.gap]||0)+1);
  return `<h2>Direct crates.io dependencies</h2><p class="muted">${D.registry.length} direct dependencies of the Explorer workspace against the newest stable release on crates.io: ${Object.entries(counts).map(([k,v])=>`${v} ${k}`).join(', ')}.</p>
  <div class="filters"><label>sort</label><select onchange="depSort=this.value;render()"><option value="gap" ${depSort==='gap'?'selected':''}>by gap</option><option value="name" ${depSort==='name'?'selected':''}>by name</option></select></div>
  <table><thead><tr><th>crate</th><th>requirement</th><th>locked</th><th>latest</th><th>released</th><th>gap</th><th>description</th></tr></thead><tbody>${rows.map(r=>`<tr><td><a href="https://crates.io/crates/${esc(r.name)}" target="_blank" rel="noreferrer">${esc(r.name)}</a></td><td>${esc(r.requirement)}</td><td>${esc(r.locked.join(', '))}</td><td>${esc(r.latest)}</td><td>${esc(r.latest_date)}</td><td><span class="badge ${r.gap==='major'?'b-security':r.gap==='minor'?'b-performance':r.gap==='patch'?'b-others':'b-features'}">${esc(r.gap)}</span></td><td class="muted">${esc(r.description)}</td></tr>`).join('')}</tbody></table>
  <h3>Crates present in more than one version</h3><table><thead><tr><th>crate</th><th>versions</th></tr></thead><tbody>${D.lock.duplicates.map(d=>`<tr><td>${esc(d.name)}</td><td>${esc(d.versions.join(', '))}</td></tr>`).join('')}</tbody></table>`;
}
function review(){ return `<div class="review">${D.review_html || '<p class="empty">No review written yet.</p>'}</div>`; }
function render(){
  renderNav(); renderSide();
  const v = document.getElementById('view');
  v.innerHTML = state.view === 'sources' ? source() : state.view === 'advisories' ? advisories() : state.view === 'dependencies' ? dependencies() : state.view === 'review' ? review() : overview();
}
render();
</script>
</body>
</html>
'''


def main():
    data = json.loads((CACHE / 'data.json').read_text(encoding='utf-8'))
    research = {}
    research_dir = OUT / 'research'
    for path in sorted(research_dir.glob('*.json')) if research_dir.exists() else []:
        try:
            entry = json.loads(path.read_text(encoding='utf-8'))
        except json.JSONDecodeError as error:
            print(f'skipping {path.name}: {error}', file=sys.stderr)
            continue
        entries = entry if isinstance(entry, list) else [entry]
        for e in entries:
            if isinstance(e, dict) and e.get('url'):
                research[e['url']] = e
    data['research'] = research
    rec_path = OUT / 'recommendations.json'
    data['recommendations'] = json.loads(rec_path.read_text(encoding='utf-8')) if rec_path.exists() else {}
    review_path = OUT / 'REVIEW.md'
    if review_path.exists():
        text = review_path.read_text(encoding='utf-8')
        data['review_html'] = markdown_to_html(text)
        first = next((p for p in text.split('\n\n') if p.strip() and not p.startswith('#')), '')
        data['review_summary'] = markdown_to_html(first)
    OUT.mkdir(parents=True, exist_ok=True)
    payload = json.dumps(data).replace('</', '<\\/')
    (OUT / 'index.html').write_text(PAGE.replace('__DATA__', payload), encoding='utf-8')
    print(f'wrote {OUT.relative_to(ROOT)}/index.html ({(OUT / "index.html").stat().st_size // 1024} KiB), research for {len(research)} source(s), {len(data["recommendations"])} recommendation(s)')
    # the artifact: the page with the research and the review it embeds, for CI uploads and sharing
    archive = CACHE / 'drift-site.tar.gz'
    with tarfile.open(archive, 'w:gz') as tar:
        for path in sorted(OUT.rglob('*')):
            if path.is_file():
                tar.add(path, arcname=str(Path('drift') / path.relative_to(OUT)))
    print(f'wrote {archive.relative_to(ROOT)} ({archive.stat().st_size // 1024} KiB)')
    return 0


if __name__ == '__main__':
    sys.exit(main())
