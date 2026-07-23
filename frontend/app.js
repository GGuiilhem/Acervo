const $ = id => document.getElementById(id);
const invoke = window.__TAURI__.core.invoke;
const { Channel } = window.__TAURI__.core;
const dialog = window.__TAURI__.dialog;
let searching = false, found = 0, flushScheduled = false, selectingAll = false;
const selected = new Set(), pending = [];

$('advanced-toggle').onclick = () => {
  const open = $('advanced-toggle').getAttribute('aria-expanded') === 'true';
  $('advanced-toggle').setAttribute('aria-expanded', String(!open));
  $('advanced').hidden = open;
};
$('browse').onclick = async () => {
  const value = await dialog.open({ directory: true, multiple: false, title: 'Selecione a pasta para pesquisar' });
  if (value) $('directory').value = value;
};
$('theme').onclick = () => {
  const light = document.documentElement.dataset.theme !== 'light';
  document.documentElement.dataset.theme = light ? 'light' : 'dark';
  $('theme').textContent = light ? '☀' : '☾';
};

function configureRange(prefix) {
  const mode = $(`${prefix}-mode`), a = $(`${prefix}-a`), b = $(`${prefix}-b`), and = $(`${prefix}-and`);
  mode.onchange = () => {
    const active = mode.value !== 'any', between = mode.value === 'between';
    a.disabled = !active; b.disabled = !between; b.hidden = !between; and.hidden = !between;
  };
}
configureRange('size'); configureRange('date');
function currentMode() { return $('search-mode').checked ? 'name' : 'content'; }
function updateMode() {
  const content = currentMode() === 'content';
  $('content-label').classList.toggle('active', content); $('name-label').classList.toggle('active', !content);
  $('replace-section').hidden = false;
  $('replace-source-field').hidden = !content;
  if (!content) $('replace-source').value = 'custom';
  const custom = !content || $('replace-source').value === 'custom';
  $('replace-query-field').hidden = !custom;
  $('replace-query').disabled = !custom || !$('enable-replace').checked;
  $('replace-help').textContent = content && !custom
    ? 'Localiza, dentro dos arquivos selecionados, o texto usado na pesquisa e troca pelo novo conteúdo. A operação sempre pede confirmação.'
    : 'Localiza este outro texto dentro dos arquivos selecionados e o troca pelo novo conteúdo, sem depender do nome ou termo usado na pesquisa.';
  document.querySelectorAll('.replace-option').forEach(button => button.hidden = !$('enable-replace').checked);
}
$('search-mode').onchange = updateMode;
$('enable-replace').onchange = () => {
  const enabled = $('enable-replace').checked;
  $('replacement').disabled = !enabled; $('replace-source').disabled = !enabled;
  $('replace-query').disabled = !enabled || $('replace-source').value !== 'custom';
  document.querySelectorAll('.replace-option').forEach(button => button.hidden = !enabled);
};
$('replace-source').onchange = () => {
  const custom = $('replace-source').value === 'custom';
  $('replace-query-field').hidden = !custom; $('replace-query').disabled = !custom || !$('enable-replace').checked;
  $('replace-help').textContent = custom
    ? 'Localiza este outro texto dentro dos arquivos selecionados e o troca pelo novo conteúdo, sem alterar o termo da pesquisa acima.'
    : 'Localiza, dentro dos arquivos selecionados, o texto usado na pesquisa e troca pelo novo conteúdo. A operação sempre pede confirmação.';
};
updateMode();

function formatSize(bytes) {
  if (!bytes) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'], i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), 3);
  return `${(bytes / 1024 ** i).toFixed(i ? 1 : 0)} ${units[i]}`;
}
function esc(value) { return value.replace(/[&<>'"]/g, c => ({ '&':'&amp;', '<':'&lt;', '>':'&gt;', "'":'&#39;', '"':'&quot;' }[c])); }
function setSearching(value) {
  searching = value; $('search').disabled = value; $('cancel').hidden = !value;
  $('search').innerHTML = value ? 'Pesquisando…' : '<span>⌕</span> Pesquisar';
}
function updateSelection() {
  $('selected-count').textContent = `${selected.size} selecionado${selected.size === 1 ? '' : 's'}`;
  $('select-all').checked = found > 0 && selected.size === found;
  $('select-all').indeterminate = selected.size > 0 && selected.size < found;
  document.querySelectorAll('#actions [data-action]').forEach(button => button.disabled = !selected.size);
}
function setRow(row, path, value) {
  row.querySelector('.row-check').checked = value; row.classList.toggle('selected', value);
  value ? selected.add(path) : selected.delete(path);
}
function toggleAll(value) {
  selectingAll = value;
  document.querySelectorAll('#results tr').forEach(row => setRow(row, row.dataset.path, value));
  updateSelection();
}
$('select-all').onchange = event => toggleAll(event.target.checked);

function appendResult(item) {
  found++; $('empty').style.display = 'none'; $('actions').hidden = false;
  const row = document.createElement('tr'); row.dataset.path = item.path;
  row.innerHTML = `<td class="select-col"><input class="row-check" type="checkbox"></td><td>${esc(item.name)}</td><td>${item.matches}</td><td>${formatSize(item.size)}</td><td>${item.modified ? new Date(item.modified * 1000).toLocaleString('pt-BR') : '—'}</td><td class="path" title="${esc(item.path)}">${esc(item.path)}</td>`;
  const check = row.querySelector('.row-check');
  check.onchange = () => { setRow(row, item.path, check.checked); selectingAll = selected.size === found; updateSelection(); };
  row.ondblclick = () => invoke('open_path', { path: item.path });
  row.oncontextmenu = event => { event.preventDefault(); if (!selected.has(item.path)) { toggleAll(false); setRow(row, item.path, true); updateSelection(); } showMenu(event.clientX, event.clientY); };
  $('results').appendChild(row); if (selectingAll) setRow(row, item.path, true);
  $('count').textContent = `${found} arquivo${found === 1 ? '' : 's'}`; updateSelection();
}
function enqueueResult(item) { pending.push(item); if (!flushScheduled) { flushScheduled = true; requestAnimationFrame(flushResults); } }
function flushResults() { flushScheduled = false; for (let i = 0; i < 100 && pending.length; i++) appendResult(pending.shift()); if (pending.length) { flushScheduled = true; requestAnimationFrame(flushResults); } }
function showMenu(x, y) { const menu = $('context-menu'); menu.hidden = false; menu.style.left = `${Math.min(x, innerWidth - 210)}px`; menu.style.top = `${Math.min(y, innerHeight - 180)}px`; }
document.addEventListener('click', () => { $('context-menu').hidden = true; });

function sizeFilters() {
  const mode = $('size-mode').value, unit = Number($('size-unit').value), a = Number($('size-a').value) * unit, b = Number($('size-b').value) * unit;
  if (mode === 'min') return [a || null, null];
  if (mode === 'max') return [null, a || null];
  if (mode === 'between') return [Math.min(a, b), Math.max(a, b)];
  return [null, null];
}
function dayValue(id, end = false) { const value = $(id).value; return value ? Math.floor(new Date(`${value}T${end ? '23:59:59' : '00:00:00'}`).getTime() / 1000) : null; }
function dateFilters() {
  const mode = $('date-mode').value;
  if (mode === 'after') return [dayValue('date-a'), null];
  if (mode === 'before') return [null, dayValue('date-a', true)];
  if (mode === 'between') return [dayValue('date-a'), dayValue('date-b', true)];
  return [null, null];
}
function requestData() {
  const [minSize, maxSize] = sizeFilters(), [minModified, maxModified] = dateFilters();
  return { directory: $('directory').value.trim(), query: $('query').value, mode: currentMode(),
    useRegex: $('regex').checked, caseSensitive: $('case').checked, wholeWord: $('whole').checked,
    includeHidden: $('hidden').checked, includeSubfolders: $('subfolders').checked, filePattern: $('pattern').value,
    minSize, maxSize, minModified, maxModified };
}
async function search() {
  if (searching) return;
  found = 0; pending.length = 0; selected.clear(); selectingAll = false; updateSelection();
  $('results').innerHTML = ''; $('actions').hidden = true; $('count').textContent = '0 arquivos'; $('empty').style.display = 'flex';
  $('empty').querySelector('strong').textContent = 'Pesquisando…'; $('empty').querySelector('span').textContent = 'Os resultados aparecerão imediatamente.';
  $('status').classList.remove('error'); setSearching(true);
  const channel = new Channel();
  channel.onmessage = event => {
    if (event.type === 'result') enqueueResult(event.item);
    else if (event.type === 'progress') $('status').textContent = `${event.scanned.toLocaleString('pt-BR')} examinados — ${event.found} encontrados`;
    else if (event.type === 'finished') { setSearching(false); $('status').textContent = event.cancelled ? 'Pesquisa cancelada' : `Concluída: ${event.scanned.toLocaleString('pt-BR')} arquivos examinados`; if (!event.found) { $('empty').querySelector('strong').textContent = 'Nenhum arquivo encontrado'; $('empty').querySelector('span').textContent = 'Tente alterar o texto ou os filtros.'; } }
  };
  try { await invoke('start_search', { req: requestData(), onEvent: channel }); }
  catch (error) { setSearching(false); $('status').textContent = String(error); $('status').classList.add('error'); }
}
$('search').onclick = search; $('cancel').onclick = () => invoke('cancel_search'); $('query').onkeydown = event => { if (event.key === 'Enter') search(); };

async function replaceSelected(paths) {
  if (!$('enable-replace').checked) { $('status').textContent = 'Ative “Pesquisar e substituir” nas opções avançadas.'; return; }
  const custom = currentMode() === 'name' || $('replace-source').value === 'custom';
  const replaceQuery = custom ? $('replace-query').value : $('query').value;
  if (!replaceQuery) { $('status').textContent = custom ? 'Informe o outro texto que deve ser localizado.' : 'Informe o texto da pesquisa.'; $('status').classList.add('error'); return; }
  if (!confirm(`Substituir o conteúdo em ${paths.length} arquivo(s) selecionado(s)?${$('backup').checked ? '\nUm backup será criado antes de cada alteração.' : '\nA opção de backup está desativada.'}`)) return;
  const changed = await invoke('replace_in_files', { req: { paths, query: replaceQuery, replacement: $('replacement').value, useRegex: $('regex').checked, caseSensitive: $('case').checked, wholeWord: $('whole').checked, createBackup: $('backup').checked } });
  $('status').textContent = `${changed} arquivo${changed === 1 ? '' : 's'} alterado${changed === 1 ? '' : 's'}.`;
}
async function action(name) {
  const paths = [...selected]; if (!paths.length) return;
  try {
    if (name === 'open') { for (const path of paths) await invoke('open_path', { path }); }
    else if (name === 'folder') {
      const count = await invoke('open_containing_folders', { paths });
      $('status').textContent = `${count} pasta${count === 1 ? '' : 's'} aberta${count === 1 ? '' : 's'}.`;
    }
    else if (name === 'zip') {
      const destination = await dialog.save({ title: 'Salvar arquivos selecionados como ZIP', defaultPath: 'resultados.zip', filters: [{ name: 'Arquivo ZIP', extensions: ['zip'] }] });
      if (!destination) return;
      const count = await invoke('zip_files', { paths, destination });
      $('status').textContent = `ZIP criado com ${count} arquivo${count === 1 ? '' : 's'}.`;
    }
    else if (name === 'replace') await replaceSelected(paths);
    else { const destination = await dialog.open({ directory: true, multiple: false, title: name === 'copy' ? 'Copiar para' : 'Mover/recortar para' }); if (!destination) return; const count = await invoke('transfer_files', { paths, destination, moveFiles: name === 'move' }); $('status').textContent = `${count} arquivo${count === 1 ? '' : 's'} ${name === 'move' ? 'movido' : 'copiado'}${count === 1 ? '' : 's'}.`; if (name === 'move') search(); }
  } catch (error) { $('status').textContent = String(error); $('status').classList.add('error'); }
}
document.querySelectorAll('[data-action]').forEach(button => button.onclick = event => { event.stopPropagation(); $('context-menu').hidden = true; action(button.dataset.action); });
