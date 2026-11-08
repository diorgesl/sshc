# SFTP Explorer — design

**Status:** aguardando revisão
**Data:** 2026-11-08
**Contexto:** sshc 0.3.3 — cliente SSH multi-sessão em Rust/egui. Este documento
descreve a primeira versão do navegador de arquivos remoto (SFTP).

## 1. Objetivo

Gerenciar arquivos do servidor **durante a sessão**: a partir de uma aba SSH
conectada, o usuário clica em **"Abrir SFTP"** e ganha uma aba de explorer com
duas panes (local e remoto), transferências em fila, edição de arquivos de
configuração e as operações completas de arquivo (criar, renomear, apagar,
permissões e links simbólicos).

Sucesso = o usuário resolve o dia a dia (ver/subir/baixar config, conferir log,
ajustar permissão) sem sair do app, sem travar a janela e sem digitar comando.

## 2. Decisões

| Tema | Decisão |
|---|---|
| Onde fica | Aba própria, aberta pelo botão no cabeçalho da aba SSH |
| Conexão | Própria (segunda sessão SSH), independente do terminal |
| Credenciais | Reusa em memória as da aba de terminal; fallback automático; diálogo se falhar |
| Layout | Local à esquerda, remoto à direita, lado a lado, divisória arrastável |
| Transferência | Fila no worker, até 3 simultâneas, progresso, cancelar, conflitos |
| Operações | Navegar, baixar, subir, criar pasta, renomear, apagar, chmod, symlinks |
| Arquitetura | Ator SFTP em crates/net, mensagens UI ⇄ worker |
| Retomada | Fora da v1; conexão caída marca jobs como interrompidos |

## 3. Fora do escopo (v1)

- Retomada de transferência, rsync, limite de banda, agendamento.
- Copiar arquivo remoto e colar no Finder/Explorer (file promise nativo do SO).
- Arrastar do app para fora (drag-out).
- SFTP em sessões Telnet/Serial.
- Recortar/mover (cut) entre panes; só copiar/colar.
- chmod no lado local e chmod recursivo.
- Favoritos/bookmarks, busca remota, comparação de pastas.
- Múltiplas abas SFTP para a mesma sessão (reabre/foca a existente).
- SFTP sem terminal aberto (não há entrada no Session Manager nesta versão).

## 4. Arquitetura

### 4.1 Refactor de autenticação (crates/net/src/ssh.rs)

Hoje ssh::connect cria a thread, monta o runtime e dentro de run faz conexão,
host key, autenticação, fallback legado e shell. O SFTP precisa do mesmo
caminho sem shell.

Extrair:

~~~rust
pub(crate) async fn open_authenticated(
    session: &Session,
    creds: Credentials,
) -> anyhow::Result<(client::Handle<HostKeyCheck>, String /* nota */)>
~~~

- Move para dentro: client_config(false), client_config(true) no fallback de
  negociação, host_key_handler, authenticate e a montagem da nota ("host key
  aprendida", "compatibilidade legada (SHA-1/CBC)").
- HostKeyCheck passa a pub(crate).
- As mensagens de erro continuam começando com "auth: " / "passphrase: ",
  porque a GUI depende de is_auth_error e encrypted_key_path.
- run() passa a chamar open_authenticated e o comportamento do terminal não
  muda. Os testes de crates/net/tests/ssh_roundtrip.rs devem continuar
  passando sem alteração.

### 4.2 Ator SFTP (crates/net/src/sftp.rs)

Novo módulo, mesmo padrão da Connection de terminal: thread própria com
runtime tokio::runtime::Builder::new_current_thread, UI síncrona conversando
por canais.

~~~rust
pub struct SftpConnection {
    cmd: tokio::sync::mpsc::UnboundedSender<SftpCmd>,
    pub events: std::sync::mpsc::Receiver<SftpEvent>,
    closed: Arc<AtomicBool>,
    pub note: String,          // host key aprendida / compatibilidade legada
    pub home: Option<String>,  // caminho inicial remoto
}
~~~

- sftp::connect(session, creds) -> anyhow::Result<SftpConnection> sobe a
  thread e espera o "pronto" por até **25 s** (mesmo timeout do terminal).
- Worker: open_authenticated → handle.channel_open_session() →
  request_subsystem(true, "sftp") → channel.into_stream() →
  russh_sftp::client::SftpSession::new(stream) (assinatura confirmada: aceita
  qualquer AsyncRead + AsyncWrite + Unpin + Send + 'static).
- SftpSession::set_timeout(Duration::from_secs(30)) para operações.
- Home: canonicalize("."); se falhar, expand_path("~"); senão None.
- SftpConnection::send(cmd) -> bool, is_closed().
- A thread é **destacada** (o JoinHandle é descartado): fechar a aba não pode
  travar a UI. Drop envia Disconnect; o worker tem até **2 s** para encerrar.
- O worker sai se: receber Disconnect, o canal de comandos fechar (sender
  dropado), o receiver de eventos cair (aba fechada) ou a sessão SSH cair.
- Se o connect estourar os 25 s, o chamador sinaliza stop e a thread encerra
  assim que terminar o handshake.

### 4.3 Protocolo

Ids (request_id, job_id) são alocados **na GUI**, monotônicos por aba; assim
não colidem depois de reconectar. O worker devolve o mesmo id.

~~~rust
pub enum SftpCmd {
    List { id: u64, path: String },
    Download { job_id: u64, sources: Vec<String>, dest: PathBuf, policy: ConflictPolicy },
    Upload { job_id: u64, sources: Vec<PathBuf>, dest: String, policy: ConflictPolicy },
    Mkdir { id: u64, path: String },
    Rename { id: u64, from: String, to: String },
    Delete { id: u64, paths: Vec<String>, recursive: bool },
    Chmod { id: u64, paths: Vec<String>, mode: u32 },
    Cancel { job_id: u64 },
    ResolveConflict { job_id: u64, decision: ConflictDecision, apply_all: bool },
    Disconnect,
}

pub enum SftpEvent {
    Listed { id: u64, path: String, result: Result<Vec<SftpEntry>, String> },
    JobStarted { id: u64, items_total: u64, bytes_total: Option<u64> },
    JobProgress {
        id: u64, items_done: u64, bytes_done: u64,
        bytes_total: Option<u64>, current: String,
    },
    Conflict {
        id: u64, direction: Direction, source: String,
        dest: String, source_size: u64, dest_size: u64,
    },
    JobDone { id: u64, outcome: JobOutcome },
    OpDone { id: u64, result: Result<(), String> },
    Closed { reason: String },
}

pub enum JobOutcome {
    Completed { ignored_links: u64 },
    Cancelled,
    Failed(String),
}

pub struct SftpEntry {
    pub name: String,
    pub path: String,                    // absoluto, POSIX
    pub kind: EntryKind,                 // File | Dir | Symlink | Other
    pub size: u64,
    pub modified: Option<u32>,           // segundos Unix
    pub mode: Option<u32>,               // bits de permissão
    pub link_target: Option<String>,
}
~~~

~~~rust
pub enum Direction { Download, Upload }
pub enum EntryKind { File, Dir, Symlink, Other }
pub enum ConflictPolicy { Ask, Overwrite, Skip }
pub enum ConflictDecision { Overwrite, Rename, Skip }
~~~

Ask é o padrão da GUI. No evento Conflict, source/dest são strings para
exibição (caminho remoto ou local, conforme a direção).

Listed.path é o caminho **resolvido** (o worker expande ~ e normaliza); a UI
usa esse valor como cwd. Listagens são executadas em tarefa própria e não
entram na fila de transferências.

### 4.4 Fila, concorrência e cancelamento

- Fila VecDeque<Job> com no máximo **3** transferências simultâneas por aba
  (JoinSet de tasks no runtime do worker).
- Um job = um arquivo ou uma pasta. Pastas são expandidas em pré-walk **antes**
  do JobStarted, somando itens e bytes; bytes_total pode ser None se o servidor
  não informar tamanho.
- Download: chunk de **64 KiB**; grava em <nome>.part no destino e renomeia no
  fim. Erro/cancelamento remove o .part.
- Upload: lê o arquivo local em chunks de 64 KiB e escreve direto no destino
  (sobrescrita confirmada antes; sem temp no servidor na v1).
- Cancel marca um AtomicBool do job, verificado a cada chunk, entre itens e
  durante o pré-walk; o job termina com JobOutcome::Cancelled.
- JobProgress é emitido no máximo **10 vezes por segundo por job**, mais um
  evento final.
- Diretório de destino existente é mesclado (padrão de gerenciador de
  arquivos); só **arquivo** gera conflito.
- Tipo incompatível no destino (arquivo onde a origem é pasta, ou vice-versa)
  vira erro do job, sem sobrescrever.
- Link simbólico:
  - em walk recursivo, **nunca** é seguido: conta em ignored_links e continua;
  - selecionado sozinho: segue o alvo apenas se for arquivo regular; link para
    pasta é recusado com mensagem clara.

### 4.5 Conflitos

- Download: conflito quando o arquivo local já existe.
- Upload: conflito quando try_exists remoto retorna verdadeiro.
- Ask: a task manda Conflict e **espera** uma decisão por canal interno; o
  worker repassa ResolveConflict. apply_all fixa a decisão para o resto do job.
- Rename: gera "stem (2).ext", "stem (3).ext"… verificando existência do lado
  de destino. A extensão é preservada quando houver.
- Job cancelado enquanto espera conflito: o canal cai e a task encerra como
  cancelada.

### 4.6 Caminhos e segurança

- Remoto é sempre POSIX (string com /), absoluto. Entrada aceita ~ (expandido
  pelo servidor via expand_path) e .. (normalizado lexicalmente no worker, sem
  resolver symlink).
- normalize_remote(path): remove barras duplicadas e finais, resolve . e ..
  sem subir acima de /. Testado por unit test.
- Download (arquivo ou pasta) usa local_dest(root, rel):
  - separa rel por /, rejeita componente vazio, . e ..;
  - monta o PathBuf e exige que o resultado comece com root;
  - erro de nome inválido no SO local (ex.: : no Windows) vira erro do item,
    **sem sanitização silenciosa**.
- Walk recursivo local usa symlink_metadata e nunca segue link (evita ciclo).
- Upload lê o arquivo local com fs::File; download nunca escreve fora da pasta
  de destino.
- Não se transfere a raiz / (botão desabilitado, mensagem "selecione uma
  pasta").

### 4.7 Ciclo de vida e reconexão

- Evento Closed { reason } quando a sessão cai sozinha.
- A aba mostra banner "conexão caiu — Reconectar"; jobs ativos viram
  Interrupted na UI.
- Reconectar cria uma nova SftpConnection com as credenciais guardadas na aba,
  re-listando o cwd remoto salvo. A fila da conexão anterior não é retomada; os
  registros continuam visíveis até "Limpar".
- Fechar aba com job ativo pede confirmação; ao fechar, a conexão é descartada
  (jobs cancelados). Fechar durante o connect descarta a conexão quando ela
  chegar.
- Se o auth falhar na reconexão, a aba mostra "Informar senha…" de novo.
- Editar ou excluir a sessão no Session Manager não mexe numa aba SFTP já
  aberta: ela guarda o próprio clone de Session e a conexão.
- Sem home remoto resolvido, a pane começa em /; sem home local, no diretório
  atual.

## 5. Interface

### 5.1 Modelo de abas

~~~rust
struct Tab {
    id: u64,
    session: Session,
    path: Option<PathBuf>,   // mesmo papel de hoje (aba por sessão)
    body: TabBody,
}

enum TabBody {
    Terminal(TerminalTab),
    Sftp(SftpTab),
}

struct TerminalTab {
    term: Terminal,
    conn: Option<Connection>,
    creds: Option<Credentials>,  // em memória, para abrir o SFTP
    sel: Option<Selection>,
    search: Option<Search>,
    log: Option<TabLog>,
    notice: Option<String>,
}

struct SftpTab {
    conn: Option<SftpConnection>,
    creds: Option<Credentials>,
    state: ConnState,      // Connecting | Connected | Closed(String) | Failed(String)
    local: PaneState<PathBuf>,
    remote: PaneState<String>,
    hidden: bool,
    split: f32,
    focus: Side,           // Local | Remote
    clip: Option<Clip>,    // { lado, caminhos }
    jobs: Vec<JobView>,
    conflicts: Vec<ConflictPrompt>,
    edits: Vec<EditedFile>,
    confirm: Option<Confirm>,
    next_id: u64,
    status: Option<String>,
}
~~~

JobView: id, direção, rótulo, estado, bytes/contadores e o spec para "Tentar de
novo". PaneState<P>: caminho (P), listagem (Loading/Ready/Error), seleção,
âncora e filtro. Confirm: excluir, fechar com jobs e sobrescrever remoto no
Editar.

- SftpTab e a view moram em crates/gui/src/sftp.rs (novo módulo), para não
  inchar ainda mais o main.rs.
- tab_index só olha abas Terminal; sftp_tab_index só Sftp (mesma regra de
  path/nome+endpoint).
- O botão do terminal devolve Action::OpenSftp; o App cria/ativa a aba.
- Pontos do main.rs que passam a casar body: barra de abas (título, ×),
  active/focus_terminal, close_tab, pump, painel central, Cmd+F (só terminal)
  e o painel de status.
- Cmd+I (filtro do Session Manager) e Cmd+F (busca) são ignorados quando um
  diálogo ou campo de texto do SFTP está com foco.
- Barra de abas: título "SFTP · <nome>"; sufixo "(N)" com o número de
  transferências ativas; foco na aba não ativa o foco do terminal.
- Fechar a aba do terminal não fecha a do SFTP (e vice-versa).

### 5.2 Abertura e credenciais

- Botão **"Abrir SFTP"** no cabeçalho de terminal_view, só quando Protocol::Ssh
  (oculto em Telnet/Serial).
- Se já existe aba SFTP para a mesma sessão (path ou nome+endpoint para quick
  connect), só ativa.
- Senão: cria a aba já em estado "conectando…", ativa e dispara sftp::connect
  numa thread (padrão PendingSftp no App, como PendingConnect).
- Credenciais: TerminalTab.creds é preenchido no finish_connect com o
  Credentials que abriu a conexão (Auto, Key ou Password); PendingConnect passa
  a carregar esse valor. O botão clona esse valor para a SftpTab. Se não houver,
  usa Auto. Nada vai para disco; a senha só vive na memória do processo/aba.
- Falha de autenticação: a aba mostra "Informar senha…" e abre o SshAuth
  existente com um alvo novo (SshAuth.sftp_tab = Some(tab_id)). Ao confirmar,
  reconecta só a aba SFTP. "Salvar no Keychain" continua funcionando.
- Sucesso: anexa a conexão, resolve home remoto e lista. Enquanto conecta, o
  botão do terminal continua visível; clicar de novo só ativa a aba que está
  conectando.
- Cancelar o SshAuth deixa a aba desconectada com o botão "Informar senha…".
  Fechar a aba durante o connect descarta a conexão quando ela chegar; se a aba
  sumiu enquanto o diálogo estava aberto, o envio é ignorado.

### 5.3 Layout

Cabeçalho: nome da sessão, endpoint, estado (conectado/conectando/caiu),
"Atualizar", "Reconectar"/"Desconectar", "Fechar".

Barra de ferramentas: "Mostrar ocultos" (global da aba), filtro de cada pane.

Corpo: split vertical redimensionável (split: f32, padrão **0.5**, limitado a
**0.25–0.75**, arrastar a divisória), **local à esquerda** e **remoto à
direita**. Cada pane tem:

- barra de caminho: breadcrumb clicável; modo texto ao clicar no lápis ou
  apertar / (Enter navega, Esc cancela). Caminho remoto aceita ~ e /; local
  aceita ~ e caminho absoluto (relativo é recusado com erro inline).
- botões: subir (também Backspace/Cmd+↑), home, atualizar, nova pasta e a ação
  da pane (local: "Enviar…"; remoto: "Baixar…") sobre a seleção.
- lista virtualizada com egui::ScrollArea::show_rows (sem dependência nova):
  colunas Nome (ícone; symlink mostra "nome → destino"), Tamanho (pasta = —),
  Modificado (hora local, AAAA-MM-DD HH:MM) e Modo (só remoto, formato
  drwxr-xr-x).
- estados: "carregando…", "pasta vazia", erro com "Tentar de novo".
- filtro por substring (case-insensitive), limpo ao trocar de pasta; mudar o
  filtro ou receber listagem nova limpa a seleção.
- ocultos e filtro afetam só a exibição; ao transferir uma pasta, o conteúdo
  inteiro (inclusive ocultos) vai junto.
- caminho digitado que não é pasta (ou não existe) mostra erro inline e não
  navega.

Ordenação fixa: pastas primeiro, depois o resto, nome case-insensitive.
Ocultos visíveis por padrão (dotfiles), alternáveis.

Rodapé: "N itens · M selecionados", última mensagem de erro/status e a fila.

### 5.4 Navegação, seleção e teclado

- Clique seleciona; Cmd/Ctrl+clique alterna; Shift+clique intervalo;
  Cmd/Ctrl+A seleciona visíveis; a pane focada ganha borda de destaque.
- Enter/duplo clique: pasta navega; arquivo remoto abre em **Editar**; arquivo
  local abre no app padrão do SO.
- Backspace/Cmd+↑ sobe; Cmd+Backspace/Delete abre a confirmação de exclusão;
  Cmd+C/Cmd+V usam o clipboard interno. Atalhos só valem com foco na pane — o
  terminal mantém os seus.
- Nova pasta: diálogo com nome; valida vazio e /.
- Renomear: edição inline na linha; valida vazio, / e nome já existente.
- Apagar: diálogo com contagem e nomes; pasta remota é recursiva, com aviso
  explícito. Local também é recursivo e permanente (sem lixeira na v1).
- Qualquer operação só afeta os itens selecionados na pane certa.

### 5.5 Arrastar, soltar e clipboard

- **Interno:** arrastar linha(s) para a outra pane enfileira transferência para
  o cwd dela; soltar sobre uma pasta usa aquela pasta como destino. Mesma pane
  ignora. Alvo fica destacado com tooltip ("Enviar 3 itens para /var/www").
- **Do SO para o app:** com a aba SFTP ativa, arquivos/pastas soltos sobre a
  pane remota enfileiram upload no cwd (ou pasta sob o ponteiro); soltar em
  outra área é ignorado. Sem conexão, a queda é ignorada com aviso no rodapé.
- **Clipboard interno:** Cmd+C numa pane guarda { lado, caminhos }; Cmd+V na
  **outra** pane enfileira a transferência; no mesmo lado, mensagem "copie para
  a outra pane". "Copiar caminho" usa o clipboard do sistema como texto (um
  caminho por linha).
- Nada de file promise/drag-out (fora do escopo).

### 5.6 Menus e operações

Menu da pane remota: Editar, Baixar para… (diálogo nativo com pasta padrão do
pane local), Criar pasta, Renomear, Permissões…, Excluir, Copiar, Copiar
caminho, Atualizar.

Menu da pane local: Abrir, Enviar para o servidor, Criar pasta, Renomear,
Excluir, Copiar, Copiar caminho, Atualizar.

Depois de mkdir/rename/delete/chmod/upload concluído, a pane afetada é
re-listada. Download concluído re-lista a pane local.

### 5.7 Fila na tela

Painel colapsável no rodapé; sem jobs, só a barra "Transferências".

- Linha: seta ↑/↓, nome (pasta = "nome (N arquivos)"), barra de bytes,
  contador i/N, estado, × e "Tentar de novo" (falhou).
- Estados: na fila, preparando, ativa, concluída, falhou, cancelada,
  interrompida.
- Sem bytes_total (servidor não informou), a barra fica indeterminada.
- Cabeçalho: "N ativas · M na fila · Limpar concluídas"; links ignorados viram
  aviso no job concluído ("3 links simbólicos ignorados").
- "Tentar de novo" reenfileira com o mesmo spec e um job_id novo.
- Jobs continuam ao trocar de aba; o pump drena os eventos e pede repaint.
- O título da aba mostra (N) enquanto houver ativas.

**Conflito:** janela modal pequena com origem/destino e tamanhos, botões
Sobrescrever / Renomear / Pular e checkbox "aplicar a todos os conflitos deste
job". Se mais de um job conflitar ao mesmo tempo, os avisos entram numa fila;
um modal por vez. O job fica pausado no arquivo até responder.

### 5.8 Editar arquivo remoto

- Duplo clique em arquivo remoto (ou menu Editar): baixa para
  <temp-do-SO>/sshc-sftp-edit/<chave-da-sessão>/<caminho-relativo>
  (chave-da-sessão = host:port:user com : trocado por _). O caminho relativo
  passa por local_dest; se não puder ser mapeado no SO, erro claro no rodapé.
- Abre no app padrão: macOS open, Windows cmd /C start "" <path>, Linux
  xdg-open. Falha ao abrir vira mensagem no rodapé.
- Guarda a foto do remoto (tamanho + mtime) e faz polling de metadata a cada
  **1 s** (só enquanto houver arquivos em edição). Mudou e ficou estável por
  **1 s** → banner "arquivo mudou — Enviar de volta / Descartar".
- "Enviar de volta": stat remoto; se o servidor mudou desde o download, avisa e
  pede confirmação. Depois enfileira upload (sobrescrevendo) e atualiza a foto
  no sucesso. **Nunca envia sozinho.**
- "Descartar": apaga a cópia temp e remove do estado.
- Vários arquivos em edição convivem (máximo **20**); banners empilhados (os
  excedentes viram "mais N"). Na inicialização, entradas com mais de **24 h**
  em sshc-sftp-edit são apagadas (não mexe no que outra instância aberta está
  usando agora).

### 5.9 Permissões

- Diálogo só na pane remota: campo octal de 3 dígitos (000–777) + 9 checkboxes
  rwx, com prévia (-rw-r--r--). Campos sincronizados; entrada inválida
  desabilita "Aplicar".
- Aplica aos selecionados; pasta aplica só nela (sem recursão na v1); bits
  especiais (setuid/sticky) não são editados. Modo inicial vem do primeiro
  selecionado; se os modos forem diferentes, o campo aparece como "—" e exige
  digitação.
- Chmod usa set_metadata; em symlink o servidor segue o alvo (sem tratamento
  especial). Sucesso re-lista; erro aparece no rodapé.

### 5.10 Persistência

Arquivo app_data_dir()/sftp.json (tmp + rename atômico):

~~~json
{
  "hidden": true,
  "split": 0.5,
  "paths": {
    "host:22:user": { "remote": "/var/www", "local": "/Users/x/proj" }
  }
}
~~~

- Chave por sessão: host:port:user (igual à do Keychain).
- Carrega no início; ausente/corrompido → defaults.
- Caminho remoto salvo é tentado; se a listagem falhar, cai para o home e
  mostra aviso. Local salvo só é usado se ainda for pasta; senão home.
- Grava ao navegar (listagem ok), soltar a divisória e alternar ocultos. Erro
  de escrita vira mensagem no rodapé, não é fatal.
- Filtros e seleção não são persistidos.

## 6. Erros

| Situação | Comportamento |
|---|---|
| DNS/recusa/timeout de conexão | erro na aba + "Reconectar" |
| Autenticação recusada | "Informar senha…" (diálogo SshAuth) |
| Host key nova / legado SHA-1 | nota no status da aba (igual terminal) |
| Servidor recusa 2ª sessão (MaxSessions) | mensagem explicando e sugerindo fechar conexão extra |
| Servidor sem subsistema sftp | erro "servidor não oferece SFTP" na aba; sem retry automático |
| Reconexão falha no auth | volta para "Informar senha…" |
| Fechar aba durante o connect | conexão é descartada ao chegar |
| Sessão caiu (Closed) | banner + jobs "interrompida"; reconectar sob demanda |
| Listagem falha | pane mostra erro + "Tentar de novo" |
| mkdir/rename/delete/chmod falha | rodapé com o motivo; lista não muda |
| Item de transferência falha | job falha com o caminho no motivo; os outros jobs seguem |
| Conflito sem resposta e job cancelado | job cancelado, modal some |
| Caminho digitado inválido | erro inline, navegação não acontece |
| Falha ao abrir editor/app | rodapé; arquivo continua baixado |
| Disco cheio | job falha, .part é removido |
| Fechar aba com job ativo | confirmação; jobs cancelados ao fechar |

O app nunca fecha nem entra em pânico por erro de SFTP; toda falha vira estado
visível.

## 7. Testes e verificação

### 7.1 Integração no crates/net (tests/sftp_roundtrip.rs)

Servidor russh local (mesmo padrão de ssh_roundtrip.rs) que autentica, aceita
subsystem_request("sftp") e roda russh_sftp::server::run sobre um tempdir (root
fixo). Casos:

- conecta, resolve home e lista;
- mkdir, rename, chmod (stat confere modo), symlink + readlink;
- upload e download de arquivo (bytes idênticos);
- upload/download de pasta recursiva (itens e bytes);
- delete recursivo e arquivo;
- conflito com Ask → Renomear gera "nome (2).ext"; Overwrite e Skip;
- cancelamento no meio de um arquivo grande deixa o destino sem .part;
- caminho remoto com .. não escapa do destino local;
- queda do servidor no meio de transferência vira Closed + job interrompido.

### 7.2 Unit tests

- sftp.rs: normalize_remote, local_dest (rejeita .., absoluto e escape),
  pré-walk/progresso, nome de conflito.
- GUI (sftp.rs/helpers): tamanho legível, modo rwx, data local, breadcrumb,
  sort (pastas primeiro), sftp.json ida-e-volta, detector de mudança do editor,
  parsing de estado.
- Manter os testes atuais de ssh_roundtrip e da GUI passando.

### 7.3 Verificação

~~~bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
~~~

Sem harness de UI automatizado: a interação é validada manualmente com um
servidor SSH local (checklist no plano de implementação).

## 8. Fatias de entrega

1. **Base de rede:** refactor open_authenticated, sftp.rs (connect + list +
   ops), testes de integração. Nada de UI ainda.
2. **Aba:** refactor TabBody, botão "Abrir SFTP", conexão/reconexão, dual pane
   com listagem e navegação local/remota.
3. **Transferências:** download/upload, fila, progresso, cancelar, conflitos,
   drag & drop e clipboard interno.
4. **Operações:** mkdir, rename, delete, chmod, symlink (exibição e regras de
   transferência), menus de contexto e confirmações.
5. **Edição e persistência:** aba Editar com polling/envio, sftp.json,
   divisória/ocultos persistidos, polimento de teclado e filtros.
6. **Fechamento:** clippy/fmt, README (seção SFTP e screenshots), revisão dos
   erros e do estado de reconexão.

Cada fatia termina com cargo test verde e a anterior utilizável.

## 9. Riscos

| Risco | Mitigação |
|---|---|
| russh-sftp 3.0.1 conversar com russh 0.65 | validar na fatia 1 com teste mínimo; API é genérica sobre AsyncRead/AsyncWrite |
| Servidor de teste (russh_sftp::server) não cobrir chmod/symlink | validar na fatia 1; se faltar, testar ops com mock de SftpSession ou contra OpenSSH local no plano |
| Diretório gigante travar a listagem | read_dir em tarefa própria; lista virtualizada; sem pré-carregar metadados extras |
| main.rs já tem ~3.8k linhas | view e estado do SFTP em crates/gui/src/sftp.rs; refactor de Tab mecânico |
| Nomes inválidos no Windows | erro claro por item, sem renomear silenciosamente |
| Sem retomada | jobs interrompidos são reenfileirados manualmente ("Tentar de novo") |
| Upload interrompido deixa arquivo parcial no servidor | v1 escreve direto (sem temp); job falha e o usuário reenvia; temp+rename fica para a v2 |
| Local em mount de rede bloqueia a UI | listagem local é síncrona na v1; documentado; mover para worker se doer |
| Nome não-UTF8 no servidor ou no disco local | russh-sftp trabalha com String; o item falha com mensagem clara em vez de corromper |

## 10. Critérios de aceite

- [ ] Botão "Abrir SFTP" aparece só em SSH e abre uma aba de explorer; clicar de
      novo foca a aba existente.
- [ ] Sem nova senha quando a sessão de terminal tem credencial em memória;
      falha cai no diálogo e "Salvar no Keychain" funciona.
- [ ] Duas panes, breadcrumb, filtro, ocultos, divisória arrastável, estado de
      carregamento/erro.
- [ ] Upload/download de arquivo e pasta, com fila, progresso, cancelar e
      conflito (sobrescrever/renomear/pular/aplicar a todos).
- [ ] Arrastar entre panes e do SO para a pane remota; Cmd+C/Cmd+V entre panes;
      copiar caminho como texto.
- [ ] mkdir, rename, delete (com confirmação), chmod e symlinks exibidos.
- [ ] Editar remoto abre no app do SO, detecta mudança, envia só com
      confirmação e avisa se o remoto mudou.
- [ ] Queda de conexão vira banner; reconectar volta ao mesmo caminho.
- [ ] Caminhos e divisória lembrados entre execuções; nenhuma senha em texto
      claro no disco.
- [ ] cargo test --workspace, clippy e fmt verdes; README atualizado.

## 11. Nomes e arquivos

- crates/net/src/sftp.rs — ator, protocolo, fila, caminhos.
- crates/net/src/ssh.rs — refactor de autenticação compartilhada.
- crates/net/tests/sftp_roundtrip.rs — integração.
- crates/gui/src/sftp.rs — SftpTab, view, fila, edição, persistência.
- crates/gui/src/main.rs — TabBody, abertura, integração de eventos.
- crates/net/Cargo.toml — russh-sftp = "3".
- crates/gui/Cargo.toml — chrono = "0.4" (data/hora local; já vem na árvore via
  russh-sftp).
