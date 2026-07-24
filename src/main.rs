#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ignore::{WalkBuilder, WalkState};
use memchr::{memchr, memchr2, memmem};
use regex::bytes::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Instant, UNIX_EPOCH},
};
use tauri::{ipc::Channel, State};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

const MAX_RESULTS: usize = 100_000;
const MAX_CONTENT_BYTES: u64 = 256 * 1024 * 1024;
const REUSABLE_BUFFER_BYTES: u64 = 1024 * 1024;

#[cfg(windows)]
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

fn worker_threads(mode: &str) -> usize {
    let logical = std::thread::available_parallelism().map_or(8, |value| value.get());
    if mode == "content" {
        logical.saturating_mul(2).clamp(8, 32)
    } else {
        logical.clamp(4, 16)
    }
}

fn read_reusing_buffer(path: &Path, buffer: &mut Vec<u8>) -> bool {
    buffer.clear();
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(FILE_FLAG_SEQUENTIAL_SCAN);
    }
    options
        .open(path)
        .and_then(|mut file| file.read_to_end(buffer))
        .is_ok()
}

fn powershell_encoded(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    BASE64.encode(bytes)
}

fn powershell_literal(value: &str) -> String {
    value.replace('\'', "''")
}

fn normal_windows_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = value.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        value.into_owned()
    }
}

fn defender_contextual_exclusion(directory: &Path) -> Result<String, String> {
    if !directory.is_absolute() || !directory.is_dir() {
        return Err("Selecione uma pasta válida antes de ativar a aceleração.".into());
    }
    if directory.parent().is_none() {
        return Err("A raiz inteira de uma unidade não pode ser excluída.".into());
    }
    let folder = normal_windows_path(
        &fs::canonicalize(directory)
            .map_err(|error| format!("Não foi possível validar a pasta: {error}"))?,
    );
    let executable = normal_windows_path(
        &std::env::current_exe()
            .map_err(|error| format!("Não foi possível localizar o Acervo: {error}"))?,
    );
    Ok(format!(
        r#"{}\:{{PathType:folder,Process:"{}"}}"#,
        folder.trim_end_matches(['\\', '/']),
        executable
    ))
}

#[cfg(windows)]
fn run_elevated_powershell(script: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let inner = powershell_encoded(script);
    let outer = format!(
        "$ErrorActionPreference='Stop'; try {{ $ps=Join-Path $env:SystemRoot 'System32\\WindowsPowerShell\\v1.0\\powershell.exe'; $p=Start-Process -FilePath $ps -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ArgumentList @('-NoProfile','-NonInteractive','-EncodedCommand','{inner}'); exit $p.ExitCode }} catch {{ exit 1 }}"
    );
    let status = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &powershell_encoded(&outer),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|error| format!("Não foi possível abrir a confirmação do Windows: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("A alteração não foi autorizada ou foi bloqueada pelo Microsoft Defender.".into())
    }
}

#[cfg(not(windows))]
fn run_elevated_powershell(_script: &str) -> Result<(), String> {
    Err("A aceleração do Microsoft Defender está disponível somente no Windows.".into())
}

#[derive(Default)]
struct AppState {
    active_search: Arc<AtomicU64>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchRequest {
    directory: String,
    query: String,
    mode: String,
    use_regex: bool,
    case_sensitive: bool,
    whole_word: bool,
    include_hidden: bool,
    include_subfolders: bool,
    pre_count: bool,
    file_pattern: String,
    min_size: Option<u64>,
    max_size: Option<u64>,
    min_modified: Option<u64>,
    max_modified: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplaceRequest {
    paths: Vec<String>,
    query: String,
    replacement: String,
    use_regex: bool,
    case_sensitive: bool,
    whole_word: bool,
    create_backup: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResult {
    name: String,
    path: String,
    size: u64,
    modified: u64,
    matches: usize,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum SearchEvent {
    Cataloging {
        discovered: usize,
    },
    Result {
        item: SearchResult,
    },
    Progress {
        scanned: usize,
        found: usize,
        total: Option<usize>,
    },
    Finished {
        scanned: usize,
        found: usize,
        cancelled: bool,
    },
}

fn regex_matcher(req: &SearchRequest) -> Result<Regex, String> {
    let mut source = if req.use_regex {
        req.query.clone()
    } else {
        regex::escape(&req.query)
    };
    if req.whole_word {
        source = format!(r"\b(?:{})\b", source);
    }
    RegexBuilder::new(&source)
        .case_insensitive(!req.case_sensitive)
        .unicode(true)
        .build()
        .map_err(|e| format!("Expressão inválida: {e}"))
}

fn replacement_matcher(req: &SearchRequest) -> Result<Regex, String> {
    let parts: Vec<String> = req
        .query
        .lines()
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(|term| {
            if req.use_regex {
                term.to_owned()
            } else {
                regex::escape(term)
            }
        })
        .collect();
    let mut source = format!("(?:{})", parts.join("|"));
    if req.whole_word {
        source = format!(r"\b{source}\b");
    }
    RegexBuilder::new(&source)
        .case_insensitive(!req.case_sensitive)
        .unicode(true)
        .build()
        .map_err(|e| format!("Expressão inválida: {e}"))
}

#[derive(Clone)]
enum SearchMatcher {
    Regex(Regex),
    Literal {
        needle: Vec<u8>,
        case_sensitive: bool,
    },
}

impl SearchMatcher {
    fn count(&self, haystack: &[u8]) -> usize {
        match self {
            Self::Regex(expression) => expression.find_iter(haystack).count(),
            Self::Literal {
                needle,
                case_sensitive: true,
            } => memmem::find_iter(haystack, needle).count(),
            Self::Literal {
                needle,
                case_sensitive: false,
            } => count_ascii_case_insensitive(haystack, needle),
        }
    }

    fn is_match(&self, haystack: &[u8]) -> bool {
        match self {
            Self::Regex(expression) => expression.is_match(haystack),
            Self::Literal {
                needle,
                case_sensitive: true,
            } => memmem::find(haystack, needle).is_some(),
            Self::Literal {
                needle,
                case_sensitive: false,
            } => count_ascii_case_insensitive(haystack, needle) > 0,
        }
    }
}

#[derive(Clone)]
enum SearchMatchers {
    Literals(AhoCorasick),
    Expressions(Vec<SearchMatcher>),
}

impl SearchMatchers {
    fn count(&self, haystack: &[u8]) -> usize {
        match self {
            Self::Literals(matcher) => matcher.find_iter(haystack).count(),
            Self::Expressions(matchers) => {
                matchers.iter().map(|matcher| matcher.count(haystack)).sum()
            }
        }
    }

    fn matching_terms(&self, haystack: &[u8]) -> usize {
        match self {
            Self::Literals(matcher) => {
                let mut matched = vec![false; matcher.patterns_len()];
                for occurrence in matcher.find_iter(haystack) {
                    matched[occurrence.pattern().as_usize()] = true;
                }
                matched.into_iter().filter(|value| *value).count()
            }
            Self::Expressions(matchers) => matchers
                .iter()
                .filter(|matcher| matcher.is_match(haystack))
                .count(),
        }
    }
}

fn count_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    let lower = needle[0].to_ascii_lowercase();
    let upper = needle[0].to_ascii_uppercase();
    let mut offset = 0;
    let mut count = 0;
    while offset + needle.len() <= haystack.len() {
        let next = if lower == upper {
            memchr(lower, &haystack[offset..])
        } else {
            memchr2(lower, upper, &haystack[offset..])
        };
        let Some(relative) = next else { break };
        let position = offset + relative;
        if position + needle.len() <= haystack.len()
            && haystack[position..position + needle.len()].eq_ignore_ascii_case(needle)
        {
            count += 1;
            offset = position + needle.len();
        } else {
            offset = position + 1;
        }
    }
    count
}

fn matcher_for_term(req: &SearchRequest, term: &str) -> Result<SearchMatcher, String> {
    if !req.use_regex && !req.whole_word && (req.case_sensitive || term.is_ascii()) {
        Ok(SearchMatcher::Literal {
            needle: term.as_bytes().to_vec(),
            case_sensitive: req.case_sensitive,
        })
    } else {
        let mut term_request = req.clone();
        term_request.query = term.to_owned();
        regex_matcher(&term_request).map(SearchMatcher::Regex)
    }
}

fn search_matchers(req: &SearchRequest) -> Result<SearchMatchers, String> {
    let mut seen = std::collections::HashSet::new();
    let terms: Vec<_> = req
        .query
        .lines()
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .filter(|term| seen.insert((*term).to_owned()))
        .collect();
    if !req.use_regex
        && !req.whole_word
        && (req.case_sensitive || terms.iter().all(|term| term.is_ascii()))
    {
        return AhoCorasickBuilder::new()
            .ascii_case_insensitive(!req.case_sensitive)
            .build(terms)
            .map(SearchMatchers::Literals)
            .map_err(|error| error.to_string());
    }
    terms
        .into_iter()
        .map(|term| matcher_for_term(req, term))
        .collect::<Result<Vec<_>, _>>()
        .map(SearchMatchers::Expressions)
}

fn wildcard_regex(patterns: &str) -> Result<Option<Regex>, String> {
    let values: Vec<_> = patterns
        .split([';', ','])
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "*")
        .collect();
    if values.is_empty() {
        return Ok(None);
    }
    let parts: Vec<String> = values
        .iter()
        .map(|value| {
            let escaped = regex::escape(value)
                .replace(r"\*", ".*")
                .replace(r"\?", ".");
            format!("(?:{escaped})")
        })
        .collect();
    RegexBuilder::new(&format!("^(?:{})$", parts.join("|")))
        .case_insensitive(true)
        .build()
        .map(Some)
        .map_err(|e| e.to_string())
}

fn validate(req: &SearchRequest) -> Result<(SearchMatchers, Option<Regex>), String> {
    if !Path::new(&req.directory).is_dir() {
        return Err("Selecione um diretório válido.".into());
    }
    if req.query.lines().all(|term| term.trim().is_empty()) {
        return Err("Digite o texto que deseja pesquisar.".into());
    }
    Ok((search_matchers(req)?, wildcard_regex(&req.file_pattern)?))
}

fn run_streaming_search(
    req: SearchRequest,
    expression: SearchMatchers,
    file_filter: Option<Regex>,
    id: u64,
    active: Arc<AtomicU64>,
    events: Channel<SearchEvent>,
) {
    let scanned = Arc::new(AtomicUsize::new(0));
    let found = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let last_progress_ms = Arc::new(AtomicU64::new(0));
    let total = if req.pre_count {
        let discovered = Arc::new(AtomicUsize::new(0));
        let last_catalog_ms = Arc::new(AtomicU64::new(0));
        let mut counter = WalkBuilder::new(&req.directory);
        counter
            .hidden(!req.include_hidden)
            .max_depth(if req.include_subfolders {
                None
            } else {
                Some(1)
            })
            .follow_links(false)
            .threads(worker_threads("name"));
        counter.build_parallel().run(|| {
            let active = active.clone();
            let discovered = discovered.clone();
            let last_catalog_ms = last_catalog_ms.clone();
            let events = events.clone();
            Box::new(move |entry| {
                if active.load(Ordering::Relaxed) != id {
                    return WalkState::Quit;
                }
                if entry
                    .ok()
                    .and_then(|value| value.file_type())
                    .is_some_and(|kind| kind.is_file())
                {
                    let current = discovered.fetch_add(1, Ordering::Relaxed) + 1;
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    let previous_ms = last_catalog_ms.load(Ordering::Relaxed);
                    if elapsed_ms.saturating_sub(previous_ms) >= 100
                        && last_catalog_ms
                            .compare_exchange(
                                previous_ms,
                                elapsed_ms,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                    {
                        let _ = events.send(SearchEvent::Cataloging {
                            discovered: current,
                        });
                    }
                }
                WalkState::Continue
            })
        });
        if active.load(Ordering::Relaxed) != id {
            let _ = events.send(SearchEvent::Finished {
                scanned: 0,
                found: 0,
                cancelled: true,
            });
            return;
        }
        Some(discovered.load(Ordering::Relaxed))
    } else {
        None
    };
    let mut builder = WalkBuilder::new(&req.directory);
    builder
        .hidden(!req.include_hidden)
        .max_depth(if req.include_subfolders {
            None
        } else {
            Some(1)
        })
        .follow_links(false)
        .threads(worker_threads(&req.mode));

    let mode = Arc::new(req.mode);
    let min_size = req.min_size;
    let max_size = req.max_size;
    let min_modified = req.min_modified;
    let max_modified = req.max_modified;
    let expression = Arc::new(expression);
    let file_filter = Arc::new(file_filter);
    let walker = builder.build_parallel();
    walker.run(|| {
        let active = active.clone();
        let scanned = scanned.clone();
        let found = found.clone();
        let last_progress_ms = last_progress_ms.clone();
        let mode = mode.clone();
        let expression = expression.clone();
        let file_filter = file_filter.clone();
        let events = events.clone();
        let mut content_buffer = Vec::with_capacity(64 * 1024);
        Box::new(move |entry| {
            if active.load(Ordering::Relaxed) != id || found.load(Ordering::Relaxed) >= MAX_RESULTS
            {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let current = scanned.fetch_add(1, Ordering::Relaxed) + 1;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let previous_ms = last_progress_ms.load(Ordering::Relaxed);
            if elapsed_ms.saturating_sub(previous_ms) >= 100
                && last_progress_ms
                    .compare_exchange(
                        previous_ms,
                        elapsed_ms,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                let _ = events.send(SearchEvent::Progress {
                    scanned: current,
                    found: found.load(Ordering::Relaxed),
                    total,
                });
            }
            let name = entry.file_name().to_string_lossy();
            if file_filter
                .as_ref()
                .as_ref()
                .is_some_and(|f| !f.is_match(name.as_bytes()))
            {
                return WalkState::Continue;
            }
            let Ok(metadata) = entry.metadata() else {
                return WalkState::Continue;
            };
            if min_size.is_some_and(|value| metadata.len() < value)
                || max_size.is_some_and(|value| metadata.len() > value)
            {
                return WalkState::Continue;
            }
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |value| value.as_secs());
            if min_modified.is_some_and(|value| modified < value)
                || max_modified.is_some_and(|value| modified > value)
            {
                return WalkState::Continue;
            }
            let matches = if mode.as_str() == "name" {
                expression.matching_terms(name.as_bytes())
            } else if metadata.len() <= MAX_CONTENT_BYTES {
                if metadata.len() <= REUSABLE_BUFFER_BYTES {
                    if read_reusing_buffer(entry.path(), &mut content_buffer) {
                        expression.count(&content_buffer)
                    } else {
                        0
                    }
                } else {
                    fs::read(entry.path()).map_or(0, |bytes| expression.count(&bytes))
                }
            } else {
                0
            };
            if matches > 0 {
                found.fetch_add(1, Ordering::Relaxed);
                let _ = events.send(SearchEvent::Result {
                    item: SearchResult {
                        name: name.into_owned(),
                        path: entry.path().to_string_lossy().into_owned(),
                        size: metadata.len(),
                        modified,
                        matches,
                    },
                });
            }
            WalkState::Continue
        })
    });
    let cancelled = active.load(Ordering::Relaxed) != id;
    let _ = events.send(SearchEvent::Finished {
        scanned: scanned.load(Ordering::Relaxed),
        found: found.load(Ordering::Relaxed),
        cancelled,
    });
}

#[tauri::command]
fn start_search(
    req: SearchRequest,
    on_event: Channel<SearchEvent>,
    state: State<AppState>,
) -> Result<u64, String> {
    let (expression, file_filter) = validate(&req)?;
    let id = state.active_search.fetch_add(1, Ordering::SeqCst) + 1;
    let active = state.active_search.clone();
    std::thread::spawn(move || {
        run_streaming_search(req, expression, file_filter, id, active, on_event)
    });
    Ok(id)
}

#[tauri::command]
fn cancel_search(state: State<AppState>) {
    state.active_search.fetch_add(1, Ordering::SeqCst);
}

#[tauri::command]
fn set_defender_acceleration(directory: String, enabled: bool) -> Result<String, String> {
    let exclusion = defender_contextual_exclusion(Path::new(&directory))?;
    let target = powershell_literal(&exclusion);
    let operation = if enabled {
        "Add-MpPreference"
    } else {
        "Remove-MpPreference"
    };
    let script = format!(
        "$ErrorActionPreference='Stop'; Import-Module Defender; $target='{target}'; $current=@((Get-MpPreference -ErrorAction Stop).ExclusionPath); if ({enabled_script}) {{ {operation} -ExclusionPath $target -Force -ErrorAction Stop }}",
        enabled_script = if enabled {
            "$current -notcontains $target"
        } else {
            "$current -contains $target"
        }
    );
    run_elevated_powershell(&script)?;
    Ok(normal_windows_path(&fs::canonicalize(directory).map_err(
        |error| format!("Não foi possível validar a pasta: {error}"),
    )?))
}

#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    open::that_detached(path).map_err(|e| format!("Não foi possível abrir: {e}"))
}

#[tauri::command]
fn open_containing_folders(paths: Vec<String>) -> Result<usize, String> {
    let folders: std::collections::HashSet<PathBuf> = paths
        .iter()
        .filter_map(|value| Path::new(value).parent().map(Path::to_path_buf))
        .collect();
    if folders.is_empty() {
        return Err("Não foi possível identificar as pastas dos arquivos.".into());
    }
    for folder in &folders {
        std::process::Command::new("explorer.exe")
            .arg(folder)
            .spawn()
            .map_err(|e| format!("Não foi possível abrir {}: {e}", folder.display()))?;
    }
    Ok(folders.len())
}

#[tauri::command]
fn available_target(destination: &Path, source: &Path) -> Result<PathBuf, String> {
    let name = source.file_name().ok_or("Nome de arquivo inválido")?;
    let direct = destination.join(name);
    if !direct.exists() {
        return Ok(direct);
    }
    let stem = source
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("arquivo");
    let ext = source
        .extension()
        .and_then(|v| v.to_str())
        .map(|v| format!(".{v}"))
        .unwrap_or_default();
    for index in 2..10_000 {
        let candidate = destination.join(format!("{stem} ({index}){ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("Não foi possível escolher um nome disponível no destino.".into())
}

fn backup_path(source: &Path) -> PathBuf {
    let direct = PathBuf::from(format!("{}.bak", source.display()));
    if !direct.exists() {
        return direct;
    }
    for index in 2..10_000 {
        let candidate = PathBuf::from(format!("{}.bak.{index}", source.display()));
        if !candidate.exists() {
            return candidate;
        }
    }
    direct
}

#[tauri::command]
fn replace_in_files(req: ReplaceRequest) -> Result<usize, String> {
    if req.paths.is_empty() {
        return Err("Selecione ao menos um arquivo.".into());
    }
    let search = SearchRequest {
        directory: ".".into(),
        query: req.query,
        mode: "content".into(),
        use_regex: req.use_regex,
        case_sensitive: req.case_sensitive,
        whole_word: req.whole_word,
        include_hidden: true,
        include_subfolders: true,
        pre_count: false,
        file_pattern: "*".into(),
        min_size: None,
        max_size: None,
        min_modified: None,
        max_modified: None,
    };
    let expression = replacement_matcher(&search)?;
    let mut changed = 0;
    for value in req.paths {
        let path = Path::new(&value);
        let original =
            fs::read(path).map_err(|e| format!("Erro ao ler {}: {e}", path.display()))?;
        if !expression.is_match(&original) {
            continue;
        }
        let replaced = if req.use_regex {
            expression.replace_all(&original, req.replacement.as_bytes())
        } else {
            expression.replace_all(
                &original,
                regex::bytes::NoExpand(req.replacement.as_bytes()),
            )
        };
        if req.create_backup {
            fs::copy(path, backup_path(path))
                .map_err(|e| format!("Erro ao criar backup de {}: {e}", path.display()))?;
        }
        fs::write(path, replaced.as_ref())
            .map_err(|e| format!("Erro ao salvar {}: {e}", path.display()))?;
        changed += 1;
    }
    Ok(changed)
}

#[tauri::command]
fn transfer_files(
    paths: Vec<String>,
    destination: String,
    move_files: bool,
) -> Result<usize, String> {
    let destination = Path::new(&destination);
    if !destination.is_dir() {
        return Err("Selecione uma pasta de destino válida.".into());
    }
    let mut completed = 0;
    for value in paths {
        let source = Path::new(&value);
        if !source.is_file() {
            continue;
        }
        let target = available_target(destination, source)?;
        if move_files {
            if fs::rename(source, &target).is_err() {
                fs::copy(source, &target)
                    .map_err(|e| format!("Erro ao mover {}: {e}", source.display()))?;
                fs::remove_file(source)
                    .map_err(|e| format!("Erro ao concluir a movimentação: {e}"))?;
            }
        } else {
            fs::copy(source, &target)
                .map_err(|e| format!("Erro ao copiar {}: {e}", source.display()))?;
        }
        completed += 1;
    }
    Ok(completed)
}

#[tauri::command]
fn zip_files(paths: Vec<String>, destination: String) -> Result<usize, String> {
    if paths.is_empty() {
        return Err("Selecione ao menos um arquivo.".into());
    }
    let destination = if destination.to_lowercase().ends_with(".zip") {
        PathBuf::from(destination)
    } else {
        PathBuf::from(format!("{destination}.zip"))
    };
    let output = fs::File::create(&destination)
        .map_err(|e| format!("Não foi possível criar {}: {e}", destination.display()))?;
    let mut archive = ZipWriter::new(output);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut used_names = std::collections::HashSet::new();
    let mut completed = 0;
    for value in paths {
        let source = Path::new(&value);
        if !source.is_file() {
            continue;
        }
        let original = source
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("arquivo");
        let mut name = original.to_string();
        let stem = source
            .file_stem()
            .and_then(|v| v.to_str())
            .unwrap_or("arquivo");
        let ext = source
            .extension()
            .and_then(|v| v.to_str())
            .map(|v| format!(".{v}"))
            .unwrap_or_default();
        let mut index = 2;
        while !used_names.insert(name.clone()) {
            name = format!("{stem} ({index}){ext}");
            index += 1;
        }
        archive
            .start_file(&name, options)
            .map_err(|e| format!("Erro ao incluir {name}: {e}"))?;
        let mut input =
            fs::File::open(source).map_err(|e| format!("Erro ao ler {}: {e}", source.display()))?;
        std::io::copy(&mut input, &mut archive)
            .map_err(|e| format!("Erro ao compactar {name}: {e}"))?;
        completed += 1;
    }
    archive
        .finish()
        .map_err(|e| format!("Erro ao finalizar o ZIP: {e}"))?;
    Ok(completed)
}

fn main() {
    tauri::Builder::default()
        .manage(AppState::default())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            start_search,
            cancel_search,
            set_defender_acceleration,
            open_path,
            open_containing_folders,
            transfer_files,
            zip_files,
            replace_in_files
        ])
        .run(tauri::generate_context!())
        .expect("erro ao executar Acervo");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_ID: AtomicUsize = AtomicUsize::new(0);
    #[test]
    fn supports_wildcards() {
        let filter = wildcard_regex("*.txt; *.md").unwrap().unwrap();
        assert!(filter.is_match(b"relatorio.TXT"));
        assert!(filter.is_match(b"notas.md"));
        assert!(!filter.is_match(b"foto.png"));
    }
    #[test]
    fn creates_case_insensitive_literal_matcher() {
        let req = SearchRequest {
            directory: ".".into(),
            query: "Pesquisa".into(),
            mode: "content".into(),
            use_regex: false,
            case_sensitive: false,
            whole_word: false,
            include_hidden: false,
            include_subfolders: true,
            pre_count: false,
            file_pattern: "*".into(),
            min_size: None,
            max_size: None,
            min_modified: None,
            max_modified: None,
        };
        assert!(regex_matcher(&req)
            .unwrap()
            .is_match("uma PESQUISA rápida".as_bytes()));
    }

    #[test]
    fn builds_a_process_scoped_defender_exclusion() {
        let directory = std::env::temp_dir().join(format!(
            "acervo-defender-test-{}",
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let exclusion = defender_contextual_exclusion(&directory).unwrap();
        assert!(exclusion.contains(r#"\:{PathType:folder,Process:""#));
        assert!(exclusion.ends_with("\"}"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn optimized_literal_counts_without_regex() {
        let req = SearchRequest {
            directory: ".".into(),
            query: "<matricula>123</matricula>".into(),
            mode: "content".into(),
            use_regex: false,
            case_sensitive: false,
            whole_word: false,
            include_hidden: false,
            include_subfolders: true,
            pre_count: false,
            file_pattern: "*.xml".into(),
            min_size: None,
            max_size: None,
            min_modified: None,
            max_modified: None,
        };
        let matcher = search_matchers(&req).unwrap();
        assert_eq!(
            matcher.count(b"<MATRICULA>123</MATRICULA><matricula>123</matricula>"),
            2
        );
        assert!(matches!(matcher, SearchMatchers::Literals(_)));
    }

    #[test]
    fn searches_multiple_terms_from_separate_lines() {
        let req = SearchRequest {
            directory: ".".into(),
            query: "111.111.111-11\n\n222.222.222-22\n111.111.111-11".into(),
            mode: "content".into(),
            use_regex: false,
            case_sensitive: false,
            whole_word: false,
            include_hidden: false,
            include_subfolders: true,
            pre_count: false,
            file_pattern: "*".into(),
            min_size: None,
            max_size: None,
            min_modified: None,
            max_modified: None,
        };
        let matchers = search_matchers(&req).unwrap();
        assert!(matches!(&matchers, SearchMatchers::Literals(value) if value.patterns_len() == 2));
        assert_eq!(matchers.count(b"111.111.111-11 e 222.222.222-22"), 2);
        assert_eq!(matchers.matching_terms(b"arquivo-222.222.222-22.xml"), 1);
    }

    #[test]
    fn replaces_selected_file_and_creates_backup() {
        let path = std::env::temp_dir().join(format!(
            "acervo-replace-{}-{}.txt",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, "texto antigo e outro texto antigo").unwrap();
        let changed = replace_in_files(ReplaceRequest {
            paths: vec![path.to_string_lossy().into_owned()],
            query: "texto antigo".into(),
            replacement: "texto novo".into(),
            use_regex: false,
            case_sensitive: false,
            whole_word: false,
            create_backup: true,
        })
        .unwrap();
        assert_eq!(changed, 1);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "texto novo e outro texto novo"
        );
        let created_backup = PathBuf::from(format!("{}.bak", path.display()));
        assert!(created_backup.exists());
        fs::remove_file(path).unwrap();
        fs::remove_file(created_backup).unwrap();
    }

    #[test]
    fn creates_zip_with_selected_files() {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("acervo-zip-{}-{id}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let first = root.join("primeiro.txt");
        let second = root.join("segundo.txt");
        let output = root.join("selecionados.zip");
        fs::write(&first, "um").unwrap();
        fs::write(&second, "dois").unwrap();
        let count = zip_files(
            vec![
                first.to_string_lossy().into_owned(),
                second.to_string_lossy().into_owned(),
            ],
            output.to_string_lossy().into_owned(),
        )
        .unwrap();
        assert_eq!(count, 2);
        let mut archive = zip::ZipArchive::new(fs::File::open(&output).unwrap()).unwrap();
        assert_eq!(archive.len(), 2);
        assert!(archive.by_name("primeiro.txt").is_ok());
        drop(archive);
        fs::remove_dir_all(root).unwrap();
    }
}
