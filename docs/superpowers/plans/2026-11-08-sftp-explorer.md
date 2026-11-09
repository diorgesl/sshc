# SFTP Explorer — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** Abrir uma aba SFTP a partir de uma sessão SSH, com panes local/remota, fila de transferências, operações completas de arquivo e edição de configs, sem travar a UI e sem senhas novas em disco.

**Architecture:** Um ator SFTP em crates/net roda numa thread com runtime tokio próprio e conversa com a GUI por canais, igual à Connection de terminal. A GUI ganha abas com corpo Terminal ou Sftp; a aba SFTP tem conexão própria, panes, fila e diálogos. I/O remoto e transferências ficam no worker; a UI só desenha estado.

**Tech Stack:** Rust 2021 (MSRV 1.75), egui/eframe 0.35, russh 0.65, russh-sftp 3, tokio 1, serde_json, chrono (formatação).

**Spec:** [2026-11-08-sftp-explorer-design.md](../specs/2026-11-08-sftp-explorer-design.md)

## Global Constraints

- Rust edition 2021, MSRV 1.75; CI roda macOS, Ubuntu e Windows.
- Novas dependências diretas: russh-sftp = "3" em crates/net; chrono = "0.4" e serde_json em crates/gui (os dois já vêm na árvore). Não usar notify, egui_extras nem walkdir.
- Conexão: timeout 25 s. Operação SFTP: timeout 30 s.
- Até 3 transferências simultâneas por aba; chunk de 64 KiB; progresso no máximo 10 vezes por segundo por job.
- Download grava .part e renomeia; erro/cancelamento remove o .part. Upload escreve direto.
- Walk recursivo nunca segue symlink; conta ignored_links.
- Remoto é POSIX absoluto; local_dest rejeita .., caminho absoluto e NUL; nunca transferir a raiz /.
- Credenciais só em memória; nenhuma senha em texto claro no disco.
- Defaults: ocultos visíveis, split 0.5 (0.25–0.75), estado em app_data_dir()/sftp.json, chave host:port:user.
- A UI não bloqueia em I/O remoto nem em transferência; listagem local é síncrona na v1.
- Excluir, renomear e sobrescrever sempre confirmam.
- Testes de symlink são cfg(unix); o resto roda nos três SOs.

## Review Focus

1. Nome remoto hostil (.., componente absoluto, NUL) não pode escapar do destino local — teste local_dest_rejects_traversal_and_nul na Task 5.
2. Symlink para pasta em upload recursivo não pode entrar em loop — teste recursive_upload_skips_symlink_loop na Task 5.
3. Arquivo de 0 byte não pode dividir por zero nem sumir — teste zero_byte_file_roundtrips na Task 5.
4. Aplicar a todos num conflito deve resolver os demais sem novo modal — teste apply_all_resolves_remaining_conflicts na Task 7.
5. Queda de conexão no meio do download deve remover o .part e emitir Closed — teste drop_mid_transfer_removes_part_and_closes na Task 6.

## File Structure

- crates/net/src/ssh.rs — extrai open_authenticated (Task 1).
- crates/net/src/sftp.rs — protocolo, ator, fila, caminhos, transferências (Tasks 2–7).
- crates/net/tests/sftp_roundtrip.rs — servidor SFTP de teste sobre tempdir + integração (Tasks 2–7).
- crates/net/Cargo.toml e crates/net/src/lib.rs — dependência e módulo (Task 2).
- crates/gui/src/main.rs — TabBody, abertura, eventos e atalhos (Tasks 8–14).
- crates/gui/src/sftp.rs — SftpTab, view, panes, fila, diálogos, edição (Tasks 9–14).
- crates/gui/Cargo.toml — chrono (Task 10).
- README.md — seção SFTP (Task 15).

## Test Helper Contracts

Nomes usados nos testes das Tasks 2–7; vivem no próprio sftp_roundtrip.rs (não são API pública):

- TestSftpServer::start(root: &Path) -> TestSftpServer; port() -> u16; control() -> TestControl; Drop aborta a task do servidor.
- TestControl: pause_reads(), resume_reads(), kill_after_bytes(n).
- recv_event(&SftpConnection, Duration) -> SftpEvent; entra em pânico no timeout.
- test_session(port: u16) -> Session SSH com usuário "teste".
- ENV_LOCK: Mutex<()> para serializar SSHC_KNOWN_HOSTS.
- op(&SftpConnection, SftpCmd) -> Result<(), String>: envia e espera OpDone com o mesmo id.
- start_download(conn, job_id, sources: Vec<String>, dest: &Path) e start_upload(conn, job_id, sources: Vec<PathBuf>, dest: &str): enviam e esperam JobDone.
- job(id: u64) -> Job: job mínimo para os testes de fila.
- entry(name: &str, kind: EntryKind) -> Entry<&'static str> e names(&[Entry<&str>]) -> Vec<String>: auxiliares do sort.
- session_sem_keychain() -> Session: sessão de teste de auth na GUI.
- Type interno Job { id, direction, sources, dest, policy, cancel: Arc<AtomicBool> }.

---

### Task 1: Extrair abertura autenticada no net

**Files:**
- Modify: crates/net/src/ssh.rs
- Test: crates/net/tests/ssh_roundtrip.rs (existente, sem mudança)

**Interfaces:**
- Consumes: nada novo.
- Produces: pub(crate) async fn open_authenticated(session: &Session, creds: Credentials) -> anyhow::Result<(client::Handle<HostKeyCheck>, String)>; HostKeyCheck passa a pub(crate).

- [ ] **Step 1: Rodar a suíte atual como baseline**

Run: cargo test -p sshc-net
Expected: PASS (os testes viram a caracterização do refactor).

- [ ] **Step 2: Extrair a função**

Mover para open_authenticated: client_config(false), o retry com client_config(true) quando is_negotiation_error, host_key_handler, authenticate e a montagem da nota (host key aprendida/legado). run() passa a chamar a função e mantém os mesmos prefixos "auth: " e "passphrase: ".

- [ ] **Step 3: Verificar**

Run: cargo test -p sshc-net
Expected: PASS, inclusive legacy_server_retries_with_sha1_and_cbc e encrypted_key_asks_for_passphrase_then_connects.

- [ ] **Step 4: Clippy**

Run: cargo clippy -p sshc-net --all-targets -- -D warnings
Expected: PASS.

- [ ] **Step 5: Commit**

~~~bash
git add crates/net/src/ssh.rs
git commit -m "refactor(net): extrai abertura SSH autenticada"
~~~

---

### Task 2: Ator SFTP com connect e listagem

**Files:**
- Create: crates/net/src/sftp.rs
- Create: crates/net/tests/sftp_roundtrip.rs
- Modify: crates/net/src/lib.rs (pub mod sftp;)
- Modify: crates/net/Cargo.toml (russh-sftp = "3")

**Interfaces:**
- Consumes: open_authenticated da Task 1.
- Produces:
  - pub fn connect(session: &Session, creds: Credentials) -> anyhow::Result<SftpConnection>
  - pub struct SftpConnection { pub events: std::sync::mpsc::Receiver<SftpEvent>, pub note: String, pub home: Option<String> }
  - send(&self, cmd: SftpCmd) -> bool; is_closed(&self) -> bool
  - pub enum SftpCmd { List { id: u64, path: String }, Disconnect }
  - pub enum SftpEvent { Listed { id: u64, path: String, result: Result<Vec<SftpEntry>, String> }, Closed { reason: String } }
  - pub struct SftpEntry { pub name: String, pub path: String, pub kind: EntryKind, pub size: u64, pub modified: Option<u32>, pub mode: Option<u32>, pub link_target: Option<String> }
  - pub enum EntryKind { File, Dir, Symlink, Other }
  - pub fn normalize_remote(path: &str) -> Option<String>
  - SftpCmd, SftpEvent, SftpEntry e EntryKind derivam Debug (testes usam {:?}).
- Fixture: TestSftpServer::start(root: &Path) -> TestSftpServer, port() -> u16, recv_event(&SftpConnection, Duration) -> SftpEvent.

- [ ] **Step 1: Escrever o teste que falha**

~~~rust
#[tokio::test]
async fn connect_lists_files() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
    std::fs::create_dir(dir.path().join("pasta")).unwrap();
    let server = TestSftpServer::start(dir.path()).await;
    std::env::set_var("SSHC_KNOWN_HOSTS", dir.path().join("known_hosts"));
    let session = test_session(server.port());
    let conn = tokio::task::spawn_blocking(move || {
        sshc_net::sftp::connect(&session, Credentials::Password("x".into()))
    }).await.unwrap().unwrap();

    assert!(conn.send(SftpCmd::List { id: 1, path: "/".into() }));
    match recv_event(&conn, Duration::from_secs(10)).await {
        SftpEvent::Listed { id: 1, path, result } => {
            assert_eq!(path, "/");
            let entries = result.unwrap();
            assert!(entries.iter().any(|e| e.name == "hello.txt" && e.kind == EntryKind::File));
            assert!(entries.iter().any(|e| e.name == "pasta" && e.kind == EntryKind::Dir));
        }
        other => panic!("esperava Listed, veio {other:?}"),
    }
}
~~~

- [ ] **Step 2: Rodar e ver falhar**

Run: cargo test -p sshc-net --test sftp_roundtrip connect_lists_files
Expected: FAIL de compilação (módulo sftp/fixture ausentes).

- [ ] **Step 3: Implementar fixture e ator**

Fixture: servidor russh que autentica senha, guarda Channel<Msg> em channel_open_session com ChannelOpenHandle e, em subsystem_request("sftp"), responde channel_success e roda russh_sftp::server::run(channel.into_stream(), SftpFs) com root no tempdir. SftpFs implementa init, realpath, opendir, readdir (lista e depois Eof), lstat, stat e close. Mapear path absoluto para root via normalize_remote, rejeitando .., caminho absoluto e NUL. Servidor sem subsistema sftp vira erro claro ("servidor não oferece SFTP").

Ator: connect sobe thread + runtime current_thread e espera ready por 25 s; set_timeout de 30 s na sessão SFTP; loop de comandos; List resolve ~, normaliza, lê read_dir e emite Listed com size/mode/modified/link_target. Home = canonicalize(".") ou expand_path("~"), senão None. Drop envia Disconnect e não dá join (thread destacada); o worker sai com Disconnect, canal de comandos fechado, receiver de eventos caído ou sessão SSH caída.

- [ ] **Step 4: Rodar até passar**

Run: cargo test -p sshc-net --test sftp_roundtrip connect_lists_files
Expected: PASS.

- [ ] **Step 5: Testar normalize_remote**

~~~rust
#[test]
fn normalize_remote_collapses_and_clamps() {
    assert_eq!(normalize_remote("/a//b/../c/").as_deref(), Some("/a/c"));
    assert_eq!(normalize_remote("/..").as_deref(), Some("/"));
    assert_eq!(normalize_remote("relativo"), None);
    assert_eq!(normalize_remote(""), None);
}
~~~

Run: cargo test -p sshc-net normalize_remote_collapses_and_clamps
Expected: PASS.

- [ ] **Step 6: Commit**

~~~bash
git add crates/net/Cargo.toml crates/net/src/lib.rs crates/net/src/sftp.rs crates/net/tests/sftp_roundtrip.rs Cargo.lock
git commit -m "feat(net): ator SFTP com connect e listagem"
~~~

---

### Task 3: Operações de metadados

**Files:**
- Modify: crates/net/src/sftp.rs
- Modify: crates/net/tests/sftp_roundtrip.rs (fixture ganha mkdir/rmdir/remove/rename/setstat/symlink/readlink)

**Interfaces:**
- Consumes: SftpConnection, SftpCmd, SftpEvent e SftpEntry da Task 2.
- Produces:
  - SftpCmd::Mkdir { id: u64, path: String }
  - SftpCmd::Rename { id: u64, from: String, to: String }
  - SftpCmd::Delete { id: u64, paths: Vec<String>, recursive: bool }
  - SftpCmd::Chmod { id: u64, paths: Vec<String>, mode: u32 }
  - SftpEvent::OpDone { id: u64, result: Result<(), String> }
  - SftpEntry.mode e link_target preenchidos na listagem.

- [ ] **Step 1: Escrever os testes que falham**

~~~rust
#[tokio::test]
async fn mkdir_rename_delete_roundtrip() {
    // server + connect
    op(&conn, SftpCmd::Mkdir { id: 1, path: "/nova".into() }).await.unwrap();
    assert!(dir.path().join("nova").is_dir());
    op(&conn, SftpCmd::Rename { id: 2, from: "/nova".into(), to: "/velha".into() }).await.unwrap();
    assert!(dir.path().join("velha").is_dir());
    op(&conn, SftpCmd::Delete { id: 3, paths: vec!["/velha".into()], recursive: true }).await.unwrap();
    assert!(!dir.path().join("velha").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn chmod_sets_mode() {
    // arquivo 0644 -> chmod 0600 -> metadata.permissions == Some(0o600)
}
~~~

- [ ] **Step 2: Rodar e ver falhar**

Run: cargo test -p sshc-net --test sftp_roundtrip mkdir_rename_delete_roundtrip chmod_sets_mode
Expected: FAIL (variantes ausentes / fixture sem métodos).

- [ ] **Step 3: Implementar no worker e na fixture**

Worker responde OpDone; Delete recursivo remove arquivos e pastas; Chmod chama set_metadata; listagem preenche mode e link_target (read_link em Symlink). Fixture implementa mkdir, rmdir, remove, rename, setstat, symlink e readlink sobre o tempdir; symlink/readlink só cfg(unix), senão OpUnsupported.

- [ ] **Step 4: Testar symlink e delete recursivo**

~~~rust
#[cfg(unix)]
#[tokio::test]
async fn symlink_listing_shows_target() {
    std::os::unix::fs::symlink("alvo.txt", dir.path().join("link")).unwrap();
    // entry link: kind == Symlink e link_target == Some("alvo.txt")
}
~~~

Run: cargo test -p sshc-net --test sftp_roundtrip
Expected: PASS.

- [ ] **Step 5: Commit**

~~~bash
git add crates/net/src/sftp.rs crates/net/tests/sftp_roundtrip.rs
git commit -m "feat(net): operações de metadados no SFTP"
~~~

---

### Task 4: Arquivo único, progresso e cancelamento

**Files:**
- Modify: crates/net/src/sftp.rs
- Modify: crates/net/tests/sftp_roundtrip.rs (fixture ganha open/read/write/fstat e controle de pausa)

**Interfaces:**
- Consumes: SftpConnection e SftpEntry.
- Produces:
  - SftpCmd::Download { job_id: u64, sources: Vec<String>, dest: PathBuf, policy: ConflictPolicy }
  - SftpCmd::Upload { job_id: u64, sources: Vec<PathBuf>, dest: String, policy: ConflictPolicy }
  - SftpCmd::Cancel { job_id: u64 }
  - SftpEvent::JobStarted { id, items_total, bytes_total: Option<u64> }
  - SftpEvent::JobProgress { id, items_done, bytes_done, bytes_total, current }
  - SftpEvent::JobDone { id, outcome: JobOutcome }
  - pub enum JobOutcome { Completed { ignored_links: u64 }, Cancelled, Failed(String) }
  - pub enum ConflictPolicy { Ask, Overwrite, Skip }
  - pub enum Direction { Download, Upload }
  - const CHUNK_SIZE: usize = 64 * 1024
- Fixture: TestControl com pause_reads/resume_reads (Arc<Notify>) para cancelamento determinístico.

- [ ] **Step 1: Escrever os testes que falham**

~~~rust
#[tokio::test]
async fn upload_download_file_roundtrip() {
    let bytes = b"conteudo".repeat(1000);
    std::fs::write(local_in.path().join("a.bin"), &bytes).unwrap();
    start_upload(&conn, 10, vec![local_in.path().join("a.bin")], "/").await;
    assert_eq!(std::fs::read(dir.path().join("a.bin")).unwrap(), bytes);

    start_download(&conn, 11, vec!["/a.bin".into()], local_out.path()).await;
    assert_eq!(std::fs::read(local_out.path().join("a.bin")).unwrap(), bytes);
}

#[tokio::test]
async fn cancel_running_download_removes_part() {
    // fixture pausa reads; Download de /slow.bin; espera JobStarted;
    // Cancel; espera JobDone::Cancelled; asserta que não há .part.
}
~~~

- [ ] **Step 2: Rodar e ver falhar**

Run: cargo test -p sshc-net --test sftp_roundtrip upload_download_file_roundtrip
Expected: FAIL (variantes/job ausentes).

- [ ] **Step 3: Implementar job de arquivo único**

Job { id, direction, sources, dest, policy, cancel: Arc<AtomicBool> }. Execução com open/read/write em chunks de 64 KiB; download para <nome>.part e rename; upload lê o arquivo local e escreve; JobStarted antes do primeiro chunk; JobProgress a cada 100 ms; JobDone no fim. Diretório como source devolve Failed("pasta ainda não suportada") até a Task 5. Fixture: open/read/write/fstat com handles; read respeita pause_reads.

- [ ] **Step 4: Rodar até passar**

Run: cargo test -p sshc-net --test sftp_roundtrip
Expected: PASS.

- [ ] **Step 5: Commit**

~~~bash
git add crates/net/src/sftp.rs crates/net/tests/sftp_roundtrip.rs
git commit -m "feat(net): transferência de arquivo único com progresso e cancelamento"
~~~

---

### Task 5: Pastas, pré-walk, zero byte e caminhos seguros

**Files:**
- Modify: crates/net/src/sftp.rs
- Modify: crates/net/tests/sftp_roundtrip.rs

**Interfaces:**
- Consumes: jobs da Task 4.
- Produces:
  - download e upload aceitam diretórios, expandidos em pré-walk antes de JobStarted;
  - pub fn local_dest(root: &Path, rel: &str) -> anyhow::Result<PathBuf>;
  - walk_local e walk_remote nunca seguem symlink e acumulam ignored_links.

- [ ] **Step 1: Escrever testes de pasta e zero byte**

~~~rust
#[tokio::test]
async fn download_directory_is_recursive() {
    std::fs::create_dir_all(dir.path().join("pasta/sub")).unwrap();
    std::fs::write(dir.path().join("pasta/a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("pasta/sub/b.txt"), b"b").unwrap();
    // Download /pasta: out/pasta/a.txt e out/pasta/sub/b.txt existem.
}

#[tokio::test]
async fn zero_byte_file_roundtrips() {
    std::fs::write(dir.path().join("vazio"), b"").unwrap();
    // Download /vazio: arquivo de 0 byte; JobDone::Completed.
}
~~~

- [ ] **Step 2: Rodar e ver falhar**

Run: cargo test -p sshc-net --test sftp_roundtrip download_directory_is_recursive zero_byte_file_roundtrips
Expected: FAIL.

- [ ] **Step 3: Implementar pré-walk e recursão**

Pré-walk conta itens e bytes (pasta vazia = 1 diretório, 0 bytes). Upload usa walk_local com symlink_metadata, sem follow; download usa read_dir remoto; ignored_links acumula. Criar diretórios no destino e mesclar pastas existentes; arquivo contra pasta (ou vice-versa) vira Failed. Link selecionado sozinho segue o alvo apenas se for arquivo regular; link para pasta vira Failed com mensagem clara. Download monta o destino com local_dest(root, rel), que rejeita componente vazio, ".", "..", NUL e prefixo/raiz. Selecionar a raiz / como origem vira Failed ("selecione uma pasta").

- [ ] **Step 4: Testar symlink e caminho hostil**

~~~rust
#[cfg(unix)]
#[tokio::test]
async fn recursive_upload_skips_symlink_loop() {
    std::os::unix::fs::symlink(local_in.path(), local_in.path().join("loop")).unwrap();
    // upload da pasta: completa, ignored_links == 1, sem loop.
}

#[test]
fn local_dest_rejects_traversal_and_nul() {
    let root = Path::new("/tmp/dest");
    assert!(local_dest(root, "../fora").is_err());
    assert!(local_dest(root, "a/../../fora").is_err());
    assert!(local_dest(root, "a\0b").is_err());
    assert!(local_dest(root, "/abs").is_err());
    assert_eq!(local_dest(root, "a/b.txt").unwrap(), root.join("a/b.txt"));
}
~~~

Run: cargo test -p sshc-net
Expected: PASS.

- [ ] **Step 5: Commit**

~~~bash
git add crates/net/src/sftp.rs crates/net/tests/sftp_roundtrip.rs
git commit -m "feat(net): transferência recursiva de pastas"
~~~

---

### Task 6: Fila, concorrência e queda de conexão

**Files:**
- Modify: crates/net/src/sftp.rs
- Modify: crates/net/tests/sftp_roundtrip.rs

**Interfaces:**
- Consumes: jobs das Tasks 4–5.
- Produces:
  - const MAX_CONCURRENT_JOBS: usize = 3;
  - struct JobQueue com push/next/finish/cancel (puro, testável);
  - Cancel de job na fila remove antes de começar;
  - Closed quando a sessão cai; job em andamento vira Failed e o .part é removido.

- [ ] **Step 1: Testar a fila pura**

~~~rust
#[test]
fn job_queue_limits_three() {
    let mut q = JobQueue::new(3);
    for id in 1..=4 { q.push(job(id)); }
    assert_eq!(q.next().unwrap().id, 1);
    assert_eq!(q.next().unwrap().id, 2);
    assert_eq!(q.next().unwrap().id, 3);
    assert!(q.next().is_none());
    q.finish();
    assert_eq!(q.next().unwrap().id, 4);
}
~~~

- [ ] **Step 2: Rodar e ver falhar**

Run: cargo test -p sshc-net job_queue_limits_three
Expected: FAIL (JobQueue ausente).

- [ ] **Step 3: Implementar JobQueue e o select do worker**

Worker mantém JobQueue + JoinSet e faz select em cmd_rx.recv() e join_next(). Ao terminar, completa a fila. Cancel na fila remove; cancel rodando seta AtomicBool. Erro de sessão emite Closed, cancela jobs e drena a fila como Failed.

- [ ] **Step 4: Testar cancelamento na fila e queda no meio**

~~~rust
#[tokio::test]
async fn queued_cancel_never_starts() {
    // enfileira 4 jobs com a fixture pausada; cancela o 4; libera;
    // o 4 nunca recebe JobStarted.
}

#[tokio::test]
async fn drop_mid_transfer_removes_part_and_closes() {
    // fixture configurada para falhar após 1 chunk; Download -> JobDone::Failed,
    // evento Closed e nenhum .part no destino.
}
~~~

Fixture: kill_after_bytes(n) faz read devolver ConnectionLost e encerra o stream.

Run: cargo test -p sshc-net --test sftp_roundtrip queued_cancel_never_starts drop_mid_transfer_removes_part_and_closes
Expected: PASS.

- [ ] **Step 5: Commit**

~~~bash
git add crates/net/src/sftp.rs crates/net/tests/sftp_roundtrip.rs
git commit -m "feat(net): fila com concorrência e queda de conexão"
~~~

---

### Task 7: Conflitos de transferência

**Files:**
- Modify: crates/net/src/sftp.rs
- Modify: crates/net/tests/sftp_roundtrip.rs

**Interfaces:**
- Consumes: jobs e Download/Upload.
- Produces:
  - SftpCmd::ResolveConflict { job_id: u64, decision: ConflictDecision, apply_all: bool }
  - SftpEvent::Conflict { id, direction, source, dest, source_size, dest_size }
  - pub enum ConflictDecision { Overwrite, Rename, Skip }
  - policy Ask pausa o job; Overwrite/Skip seguem sem perguntar.

- [ ] **Step 1: Testar conflito de download com Rename**

~~~rust
#[tokio::test]
async fn download_conflict_rename_creates_numbered_file() {
    // remoto /a.txt; local a.txt já existe
    // JobStarted -> Conflict -> Resolve(Rename) -> JobDone::Completed
    // local: "a (2).txt" com o conteúdo remoto.
}
~~~

- [ ] **Step 2: Rodar e ver falhar**

Run: cargo test -p sshc-net --test sftp_roundtrip download_conflict_rename_creates_numbered_file
Expected: FAIL.

- [ ] **Step 3: Implementar espera por decisão**

Cada job tem um mpsc de decisões; a task envia Conflict e await. O worker repassa ResolveConflict. apply_all fixa a decisão no job. Rename procura o primeiro nome livre (stem + " (n)" + ext) no destino. Overwrite sobrescreve; Skip pula o arquivo. Cancelar um job que espera conflito fecha o canal de decisões e encerra como Cancelled.

- [ ] **Step 4: Testar apply_all e skip**

~~~rust
#[tokio::test]
async fn apply_all_resolves_remaining_conflicts() {
    // dois arquivos em conflito; primeira resposta Overwrite + apply_all;
    // deve chegar só UM evento Conflict e JobDone::Completed.
}
~~~

Run: cargo test -p sshc-net --test sftp_roundtrip
Expected: PASS.

- [ ] **Step 5: Commit**

~~~bash
git add crates/net/src/sftp.rs crates/net/tests/sftp_roundtrip.rs
git commit -m "feat(net): resolução de conflitos na transferência"
~~~

---

### Task 8: Separar o corpo de terminal da aba

**Files:**
- Modify: crates/gui/src/main.rs (Tab, tab_index, close_tab, pump, tab bar, painel central, Cmd+F)

**Interfaces:**
- Consumes: nada do SFTP ainda.
- Produces:
  - struct Tab { id: u64, session: Session, path: Option<PathBuf>, body: TabBody }
  - enum TabBody { Terminal(TerminalTab) } — a variante Sftp entra na Task 9.
  - struct TerminalTab { term, conn, creds: Option<Credentials>, sel, search, log, notice }
  - tab_index considera só Terminal.

- [ ] **Step 1: Baseline da GUI**

Run: cargo test -p sshc-gui
Expected: PASS.

- [ ] **Step 2: Refatorar o modelo**

Mover os campos de terminal para TerminalTab; manter id/session/path no Tab. Trocar acessos diretos (tab.conn, tab.term, tab.sel, tab.search, tab.log, tab.notice) por match no body. Ajustar close_tab, pump, barra de abas, painel central, Cmd+F e Cmd+I. Nenhum comportamento novo.

- [ ] **Step 3: Verificar**

Run: cargo test -p sshc-gui
Expected: PASS.
Run: cargo clippy -p sshc-gui --all-targets -- -D warnings
Expected: PASS.

- [ ] **Step 4: Commit**

~~~bash
git add crates/gui/src/main.rs
git commit -m "refactor(gui): separa o corpo de terminal da aba"
~~~

---

### Task 9: Adicionar corpo Sftp, botão e credenciais

**Files:**
- Create: crates/gui/src/sftp.rs
- Modify: crates/gui/src/main.rs (TabBody::Sftp, mod sftp; Action::OpenSftp; PendingSftp; SshAuth.sftp_tab; sftp_tab_index)

**Interfaces:**
- Consumes: sshc_net::sftp::{connect, SftpConnection}; Credentials; SshAuth.
- Produces:
  - enum TabBody { Terminal(TerminalTab), Sftp(SftpTab) } — adiciona a variante e o módulo.
  - struct SftpTab { conn: Option<SftpConnection>, creds: Option<Credentials>, state: ConnState, note: Option<String> } — nesta task, só isso e helpers.
  - enum ConnState { Connecting, Connected, Closed(String), Failed(String) }
  - enum AuthTarget { Terminal, Sftp(u64) }
  - enum SftpAction { None, Reconnect, Close, NeedAuth }; fn sftp_view(ui: &mut egui::Ui, tab: &mut SftpTab) -> SftpAction (o App aplica depois do painel central).
  - Action::OpenSftp; App::open_sftp(session, path, creds)
  - fn sftp_tab_index(&self, path: &Option<PathBuf>, session: &Session) -> Option<usize>
  - fn start_sftp_connect(tab_id: u64, creds: Credentials, save_password: Option<(String, String)>)
  - fn reconnect_sftp(tab_id: u64)
  - struct PendingSftp { tab_id: u64, rx: std::sync::mpsc::Receiver<Result<SftpConnection, String>> }
  - SshAuth.sftp_tab: Option<u64>; fn auth_target(&SshAuth) -> AuthTarget
  - TerminalTab.creds preenchido no finish_connect a partir de PendingConnect.creds.

- [ ] **Step 1: Testar o roteamento de auth**

~~~rust
#[test]
fn auth_target_routes_to_sftp_when_tab_id_is_set() {
    let mut auth = SshAuth::new(session_sem_keychain());
    auth.sftp_tab = Some(7);
    assert_eq!(auth_target(&auth), AuthTarget::Sftp(7));
    auth.sftp_tab = None;
    assert_eq!(auth_target(&auth), AuthTarget::Terminal);
}
~~~

Run: cargo test -p sshc-gui auth_target_routes_to_sftp_when_tab_id_is_set
Expected: FAIL (função ausente).

- [ ] **Step 2: Implementar botão e abertura da aba**

Em terminal_view, mostrar "Abrir SFTP" só para Protocol::Ssh e devolver Action::OpenSftp. App::open_sftp ativa a aba existente (mesmo path, ou nome+endpoint no quick connect) ou cria SftpTab Connecting, ativa e chama start_sftp_connect. A barra de abas mostra "SFTP · <nome>". A aba guarda um clone da Session: editar ou excluir a sessão no Session Manager não mexe nela, e fechar o terminal não fecha o SFTP.

- [ ] **Step 3: Conectar, credenciais e pump**

start_sftp_connect sobe a thread e guarda PendingSftp. No pump: sucesso anexa conn, guarda note, lista a partir de conn.home (ou /) e salva no Keychain se pedido; se a aba não tiver creds, usa Credentials::Auto; PendingConnect passa a carregar creds e finish_connect preenche TerminalTab.creds. Auth falha habilita "Informar senha…"; erro comum vira Failed com mensagem amigável (inclusive MaxSessions); SftpEvent::Closed vira Closed(reason), mostra note e "Reconectar" (reconnect_sftp reusa creds).

- [ ] **Step 4: Estender SshAuth**

Adicionar sftp_tab. No submit, auth_target decide entre terminal e start_sftp_connect. Salvar no Keychain continua igual. Cancelar fecha o diálogo; fechar a aba durante o connect descarta a conexão ao chegar.

- [ ] **Step 5: Verificar**

Run: cargo test -p sshc-gui
Expected: PASS.
Run: cargo check -p sshc-gui
Expected: PASS.

- [ ] **Step 6: Commit**

~~~bash
git add crates/gui/src/main.rs crates/gui/src/sftp.rs
git commit -m "feat(gui): aba SFTP com conexão própria"
~~~

---

### Task 10: Panes local e remota com navegação

**Files:**
- Modify: crates/gui/src/sftp.rs
- Modify: crates/gui/src/main.rs (render do corpo, guarda de atalhos)
- Modify: crates/gui/Cargo.toml (chrono = "0.4")

**Interfaces:**
- Consumes: SftpCmd::List e SftpEvent::Listed; ConnState; sftp_view/SftpAction da Task 9.
- Produces:
  - SftpTab ganha local: PaneState<PathBuf>, remote: PaneState<String>, hidden: bool, split: f32, focus: Side.
  - enum Side { Local, Remote }
  - struct Entry<P> { name, path: P, kind: EntryKind, size, modified, mode, link_target }
  - enum Listing<P> { Loading, Ready(Vec<Entry<P>>), Error(String) }
  - struct PaneState<P> { cwd: P, listing: Listing<P>, selected: BTreeSet<String>, anchor: Option<usize>, filter: String }
  - pub fn sort_entries<P>(entries: &mut Vec<Entry<P>>)
  - pub fn breadcrumb(path: &str) -> Vec<String>
  - pub fn human_size(bytes: u64) -> String
  - pub fn mode_string(mode: u32, is_dir: bool) -> String
  - pub fn validate_remote_input(input: &str) -> Option<String>
  - pub fn validate_local_input(input: &str, home: &Path) -> Option<PathBuf>

- [ ] **Step 1: Testar os helpers puros**

~~~rust
#[test]
fn sort_puts_dirs_first_case_insensitive() {
    let mut v = vec![entry("zeta.txt", EntryKind::File), entry("Alfa", EntryKind::Dir), entry("beta", EntryKind::File)];
    sort_entries(&mut v);
    assert_eq!(names(&v), vec!["Alfa", "beta", "zeta.txt"]);
}

#[test]
fn breadcrumb_keeps_root_and_segments() {
    assert_eq!(breadcrumb("/a/b"), vec!["/", "a", "b"]);
    assert_eq!(breadcrumb("/"), vec!["/"]);
}

#[test]
fn validate_remote_rejects_relative() {
    assert!(validate_remote_input("relativo").is_none());
    assert_eq!(validate_remote_input("/var/www").as_deref(), Some("/var/www"));
}

#[test]
fn mode_string_renders_rwx() {
    assert_eq!(mode_string(0o644, false), "-rw-r--r--");
    assert_eq!(mode_string(0o755, true), "drwxr-xr-x");
}
~~~

Run: cargo test -p sshc-gui
Expected: FAIL (helpers ausentes).

- [ ] **Step 2: Implementar cabeçalho, toolbar e divisória**

Cabeçalho da aba (nome, endpoint, estado, Atualizar, Reconectar/Desconectar, Fechar); toolbar com "Mostrar ocultos" e filtros; split vertical arrastável com clamp 0.25–0.75 (padrão 0.5).

- [ ] **Step 3: Implementar as listas**

Cada pane com breadcrumb/path edit, subir/home/atualizar/nova pasta/ação; ScrollArea::show_rows com colunas Nome/Tamanho/Modificado/Modo (modo só remoto); estados carregando/vazio/erro; filtro por substring; ocultos global.

- [ ] **Step 4: Ligar listagem, navegação e interação**

Local: std::fs::read_dir e symlink_metadata. Remoto: SftpCmd::List; pump aplica Listed e atualiza PaneState. Navegar limpa seleção/filtro; refresh manual; erro mostra "Tentar de novo". Seleção com clique/Cmd/Shift e Cmd+A para os visíveis; Enter/duplo clique (arquivo local abre no app do SO; arquivo remoto abre em Editar); Backspace sobe; Cmd+F/Cmd+I ignorados com foco na pane ou diálogo. Copiar/colar entra na Task 11. Persistência de caminho fica para a Task 14.

- [ ] **Step 5: Verificar**

Run: cargo test -p sshc-gui
Expected: PASS.
Run: cargo check -p sshc-gui
Expected: PASS.

- [ ] **Step 6: Commit**

~~~bash
git add crates/gui/Cargo.toml crates/gui/src/main.rs crates/gui/src/sftp.rs Cargo.lock
git commit -m "feat(gui): panes local e remota com navegação"
~~~

---

### Task 11: Fila na tela, arrastar e colar

**Files:**
- Modify: crates/gui/src/sftp.rs
- Modify: crates/gui/src/main.rs (pump de eventos de job)

**Interfaces:**
- Consumes: Download/Upload/Cancel, JobStarted/JobProgress/JobDone/Conflict; panes da Task 10.
- Produces:
  - enum JobState { Queued, Running, Done, Failed, Cancelled, Interrupted }
  - struct JobView { id, direction, label, state, bytes_done, bytes_total, items_done, items_total, ignored_links, error, retry: RetrySpec }
  - enum RetrySpec { Download { sources: Vec<String>, dest: PathBuf }, Upload { sources: Vec<PathBuf>, dest: String } }
  - enum ClipPaths { Local(Vec<PathBuf>), Remote(Vec<String>) }
  - struct Clip { side: Side, paths: ClipPaths }
  - pub fn paste_target(side: Side, clip: &Clip) -> Option<Side>
  - pub fn drop_destination(cwd: &str, hovered_folder: Option<&str>) -> String
  - enum Confirm { CloseTab { id: u64 } } — a Task 12 adiciona Delete e OverwriteRemote.
  - struct ConflictPrompt { job_id: u64, direction: Direction, source: String, dest: String, source_size: u64, dest_size: u64 }
  - SftpTab ganha jobs: Vec<JobView>, conflicts: Vec<ConflictPrompt>, clip: Option<Clip>, confirm: Option<Confirm>, next_id: u64 e status: Option<String> nesta task. GUI aloca ids monotônicos para List/ops/jobs.

- [ ] **Step 1: Testar os helpers**

~~~rust
#[test]
fn paste_only_crosses_sides() {
    let clip = Clip { side: Side::Remote, paths: ClipPaths::Remote(vec!["/a".into()]) };
    assert_eq!(paste_target(Side::Local, &clip), Some(Side::Remote));
    assert_eq!(paste_target(Side::Remote, &clip), None);
}

#[test]
fn drop_on_folder_uses_folder() {
    assert_eq!(drop_destination("/home", Some("/home/sub")), "/home/sub");
    assert_eq!(drop_destination("/home", None), "/home");
}
~~~

- [ ] **Step 2: Implementar o painel da fila e os ids**

A GUI aloca next_id para List, ops e jobs; eventos são correlacionados por id (inclusive depois de reconectar). Painel colapsável; linhas com direção, nome, barra, i/N, estado, cancelar e tentar de novo; cabeçalho com contagem; título da aba com (N) ativo; barra indeterminada quando bytes_total é None. JobDone::Completed com ignored_links > 0 mostra "N links simbólicos ignorados". Download concluído re-lista a pane local; upload concluído re-lista a remota. SftpEvent::Closed marca jobs ativos como Interrupted. No pump do App, drenar eventos e chamar ctx.request_repaint() enquanto houver job ativo.

- [ ] **Step 3: Implementar o modal de conflito**

Um modal por vez, com origem/destino, tamanhos, Sobrescrever/Renomear/Pular e apply_all; o job fica pausado até responder.

- [ ] **Step 4: Implementar drag & drop**

Arrastar linhas entre panes via payload do egui; soltar em pasta usa drop_destination; OS dropped_files na pane remota enfileira upload; soltar fora da pane remota é ignorado.

- [ ] **Step 5: Implementar clipboard e fechamento**

Cmd+C guarda Clip; Cmd+V na outra pane enfileira; "Copiar caminho" copia texto; fechar aba com job ativo usa Confirm::CloseTab e desconecta.

- [ ] **Step 6: Verificar**

Run: cargo test -p sshc-gui
Expected: PASS.
Run: cargo check -p sshc-gui
Expected: PASS.

- [ ] **Step 7: Commit**

~~~bash
git add crates/gui/src/main.rs crates/gui/src/sftp.rs
git commit -m "feat(gui): fila de transferências, arrastar e colar"
~~~

---

### Task 12: Operações de arquivo e diálogos

**Files:**
- Modify: crates/gui/src/sftp.rs
- Modify: crates/gui/src/main.rs (pump de OpDone)

**Interfaces:**
- Consumes: Mkdir/Rename/Delete/Chmod e OpDone; panes da Task 10.
- Produces:
  - enum Confirm { Delete { remote: bool, paths: Vec<String> }, CloseTab { id: u64 }, OverwriteRemote { edit: usize } } — CloseTab vem da Task 11; esta task adiciona os outros dois.
  - pub fn parse_octal(input: &str) -> Option<u32>
  - pub fn format_octal(mode: u32) -> String
  - pub fn valid_entry_name(name: &str) -> bool
  - Menus de contexto e diálogos de nova pasta/renomear/excluir/permissões.

- [ ] **Step 1: Testar helpers**

~~~rust
#[test]
fn octal_roundtrips() {
    assert_eq!(parse_octal("644"), Some(0o644));
    assert_eq!(parse_octal("0"), Some(0));
    assert_eq!(parse_octal("888"), None);
    assert_eq!(parse_octal("64"), None);
    assert_eq!(format_octal(0o755), "755");
}

#[test]
fn name_rejects_slash_and_empty() {
    assert!(!valid_entry_name(""));
    assert!(!valid_entry_name("a/b"));
    assert!(valid_entry_name("a b.txt"));
}
~~~

- [ ] **Step 2: Implementar menus e diálogos**

Menus remoto/local conforme o spec; "Baixar para…" usa seletor de pasta (padrão = cwd local) e "Enviar…" usa seletor de arquivos/pasta; Cmd+Backspace/Delete abre a confirmação de exclusão com contagem/nomes; renomear inline; nova pasta; permissões (só remoto, pasta sem recursão, symlink segue o alvo) com octal + checkboxes rwx e prévia (seleção mista mostra —); bits especiais (setuid/sticky) não são editados; aplicar manda Chmod. Após OpDone ok, re-listar a pane afetada; erro vai ao rodapé.

- [ ] **Step 3: Verificar**

Run: cargo test -p sshc-gui
Expected: PASS.
Run: cargo check -p sshc-gui
Expected: PASS.

- [ ] **Step 4: Commit**

~~~bash
git add crates/gui/src/main.rs crates/gui/src/sftp.rs
git commit -m "feat(gui): operações de arquivo no painel remoto"
~~~

---

### Task 13: Editar arquivo remoto

**Files:**
- Modify: crates/gui/src/sftp.rs
- Modify: crates/gui/src/main.rs (pump do polling)

**Interfaces:**
- Consumes: Download/Upload; panes da Task 10; Confirm::OverwriteRemote da Task 12; local_dest de sshc_net::sftp.
- Produces:
  - SftpTab ganha edits: Vec<EditedFile>.
  - struct EditedFile { remote: String, local: PathBuf, remote_size: u64, remote_mtime: Option<u32>, local_size: u64, local_mtime: std::time::SystemTime, dirty: bool, stable_since: Option<std::time::Instant> }
  - pub fn edit_temp_path(root: &Path, key: &str, remote: &str) -> anyhow::Result<PathBuf>
  - pub fn changed_since(size: u64, mtime: Option<u32>, snapshot_size: u64, snapshot_mtime: Option<u32>) -> bool
  - open_with_os(path: &Path) -> std::io::Result<()>

- [ ] **Step 1: Testar caminho temporário e detecção**

~~~rust
#[test]
fn edit_path_stays_under_root() {
    let root = Path::new("/tmp/edit");
    let p = edit_temp_path(root, "host_22_user", "/etc/nginx/nginx.conf").unwrap();
    assert_eq!(p, root.join("host_22_user/etc/nginx/nginx.conf"));
    assert!(edit_temp_path(root, "k", "/../../fora").is_err());
}

#[test]
fn changed_since_compares_size_or_mtime() {
    assert!(!changed_since(10, Some(5), 10, Some(5)));
    assert!(changed_since(11, Some(5), 10, Some(5)));
    assert!(changed_since(10, Some(6), 10, Some(5)));
}
~~~

- [ ] **Step 2: Implementar edição**

Duplo clique/menu Editar: baixa para <temp>/sshc-sftp-edit/<key>/<rel> (local_dest), abre com open/xdg-open/start; guarda snapshot; polling de 1 s enquanto houver arquivos; mudança estável por 1 s vira dirty; banner com Enviar de volta/Descartar; antes de enviar, stat remoto e Confirm::OverwriteRemote se mudou; upload sobrescreve e atualiza snapshot. No máximo 20 arquivos em edição; limpeza de entradas com mais de 24 h na inicialização.

- [ ] **Step 3: Verificar**

Run: cargo test -p sshc-gui
Expected: PASS.
Run: cargo check -p sshc-gui
Expected: PASS.

- [ ] **Step 4: Commit**

~~~bash
git add crates/gui/src/main.rs crates/gui/src/sftp.rs
git commit -m "feat(gui): editar arquivo remoto com envio confirmado"
~~~

---

### Task 14: Persistência do estado do SFTP

**Files:**
- Modify: crates/gui/src/sftp.rs
- Modify: crates/gui/src/main.rs (carregar no App::new, salvar nas transições)
- Modify: crates/gui/Cargo.toml (serde_json.workspace = true)

**Interfaces:**
- Consumes: PaneState, Side, split, hidden.
- Produces:
  - struct SavedPath { remote: String, local: String }
  - struct SftpState { hidden: bool, split: f32, paths: BTreeMap<String, SavedPath> }
  - fn load_sftp_state(path: &Path) -> SftpState (ausente/corrompido -> hidden=true, split=0.5)
  - fn save_sftp_state(path: &Path, state: &SftpState) -> anyhow::Result<()> (tmp + rename)
  - fn session_path_key(session: &Session) -> String (host:port:user)
  - fn apply_saved_paths(tab: &mut SftpTab, state: &SftpState, session: &Session)

- [ ] **Step 1: Testar roundtrip e defaults**

~~~rust
#[test]
fn sftp_state_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sftp.json");
    let mut state = SftpState::default();
    state.paths.insert("h:22:u".into(), SavedPath { remote: "/var".into(), local: "/tmp".into() });
    save_sftp_state(&path, &state).unwrap();
    let back = load_sftp_state(&path);
    assert_eq!(back.paths["h:22:u"].remote, "/var");
    assert_eq!(back.split, 0.5);
    assert!(back.hidden);
}

#[test]
fn corrupt_state_falls_back_to_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sftp.json");
    std::fs::write(&path, "{").unwrap();
    let state = load_sftp_state(&path);
    assert_eq!(state.split, 0.5);
    assert!(state.hidden);
}
~~~

- [ ] **Step 2: Implementar**

Ler/gravar app_data_dir()/sftp.json com serde_json; impl Default manual com hidden=true e split=0.5 (derive daria false/0.0); salvar atômico. Carregar no App::new. Aplicar caminho salvo: local se for pasta, senão home; remoto tenta listar e cai no home com aviso. Salvar ao navegar, ao soltar a divisória e ao alternar ocultos; erro vira status, não fatal.

- [ ] **Step 3: Verificar**

Run: cargo test -p sshc-gui
Expected: PASS.

- [ ] **Step 4: Commit**

~~~bash
git add crates/gui/src/main.rs crates/gui/src/sftp.rs
git commit -m "feat(gui): persistência do estado do SFTP"
~~~

---

### Task 15: README e verificação final

**Files:**
- Modify: README.md
- Verify: workspace inteiro

**Interfaces:**
- Consumes: tudo das Tasks 1–14.
- Produces: seção SFTP no README e checklist de aceite executado.

- [ ] **Step 1: Atualizar o README**

Seção "SFTP" com: botão Abrir SFTP, aba própria/conexão própria, panes, arrastar/colar, fila, conflitos, editar config, permissões, caminhos lembrados. Dizer que arquivo remoto não aparece no Finder/Explorer (sem file promise) e que a v1 não retoma transferência.

- [ ] **Step 2: Formatação, clippy e testes**

~~~bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
~~~

Expected: tudo PASS.

- [ ] **Step 3: Checklist manual com um servidor SSH local**

1. Clicar Abrir SFTP numa aba SSH e conectar sem novo prompt.
2. Navegar nas panes, alternar ocultos, filtrar e editar caminho.
3. Subir/baixar arquivo e pasta; ver progresso e cancelar.
4. Provocar conflito e testar Sobrescrever/Renomear/Pular/aplicar a todos.
5. Arrastar entre panes e do Finder/Explorer para a pane remota; Cmd+C/Cmd+V.
6. Criar pasta, renomear, apagar e mudar permissões.
7. Editar remoto, salvar no editor, conferir banner e enviar de volta.
8. Derrubar e reconectar; conferir jobs interrompidos e caminho restaurado.
9. Fechar e reabrir o app; conferir caminhos e divisória lembrados.

- [ ] **Step 4: Commit**

~~~bash
git add README.md
git add -u
git commit -m "docs: seção SFTP no README e verificação final"
~~~

---

## Manual Acceptance (do spec)

- [ ] Abrir SFTP só em SSH; foca aba existente.
- [ ] Credenciais em memória, fallback automático, diálogo e Keychain.
- [ ] Duas panes, breadcrumb, filtro, ocultos, split, estados.
- [ ] Transferência de arquivo/pasta com fila, progresso, cancelar e conflitos.
- [ ] Drag & drop interno e do SO; clipboard entre panes; copiar caminho.
- [ ] mkdir/rename/delete/chmod/symlink.
- [ ] Editar com detecção e envio confirmado.
- [ ] Queda/reconexão; persistência.
- [ ] cargo test/clippy/fmt verdes.


