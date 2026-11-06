# sshc

Cliente SSH multi-sessão em Rust (protótipo). Marca provisória: `sshc`.

## Sessões na nuvem: login Google

`sshc login` abre o navegador (OAuth 2.0 PKCE, loopback), você autoriza, e o
app grava as sessões num **app-data folder privado** do seu Google Drive
(`drive.appdata`): ele só enxerga os arquivos que ele mesmo criou — o resto do
Drive fica invisível, sem permissão de leitura.

Noutro PC: instala, `sshc login`, `sshc sync` — as sessões descem.

```bash
cargo run -- login --client-id <SEU_CLIENT_ID>   # guarda em ~/.sshc/client-id
cargo run -- sync --dry-run                      # mostra o que faria
cargo run -- sync                                # reconcilia
cargo run -- logout                              # apaga o token do Keychain
```

O refresh token fica no **Keychain do macOS**, nunca em arquivo.

### Criando o client ID (5 minutos, uma vez)

1. <https://console.cloud.google.com> → projeto novo (ex.: `sshc`)
2. **APIs & Services → Library** → ative **Google Drive API**
3. **OAuth consent screen** → External → preencha nome/email → Publishing status
   **Publish app** (o escopo `drive.appdata` não é sensível, não pede verificação).
   Se ficar em *Testing*, o refresh token expira em 7 dias.
4. **Credentials → Create credentials → OAuth client ID → Desktop app**
5. Copie o ID (`....apps.googleusercontent.com`):
   `sshc login --client-id <ID>`

Redirect usa loopback `http://127.0.0.1:<porta>`, permitido para clientes desktop.

## Interface

```bash
cargo run -p sshc-gui
```

![interface](docs/screenshot.png)

Árvore de sessões por pasta, filtro por nome/host/grupo, painel de terminal com
teclado, e importação do SecureCRT pela própria janela (com seletor de pasta).

**Conecta de verdade:** SSH (chave ou senha, PTY e resize), Telnet (filtro IAC
recusando opções) e Serial (porta + baud).

![login SSH](docs/screenshot-ssh.png)

O SSH pede credenciais num diálogo (usuário, chave ou senha) e **não grava
senha**. A host key é verificada contra o `~/.ssh/known_hosts`; se for nova,
é registrada e a impressão digital aparece na barra de status (TOFU). Para usar
outro arquivo: `SSHC_KNOWN_HOSTS=/caminho`.

Um teste de fumaça abre, conecta num servidor Telnet local, salva a tela em
`/tmp/sshc-gui.ppm` e fecha — é assim que a captura acima é gerada.

### Adicionar conexões

| Como | O que faz |
|---|---|
| **+ Nova sessão** | formulário: nome, grupo, protocolo, host, porta (ou baud), usuário, emulação |
| **Quick connect** | conecta na hora, sem salvar nada |
| **clique direito** na sessão | Editar · Duplicar · Excluir (com confirmação) |
| **Importar SecureCRT…** | traz as sessões do SecureCRT (com seletor de pasta) |
| arquivo `.toml` na store | `sshc where` mostra o caminho; pastas = grupos |

![cadastro de sessão](docs/screenshot-form.png)

Modo smoke para CI/captura: `SSHC_GUI_SMOKE=1` abre, conecta na primeira
sessão, salva `/tmp/sshc-gui.ppm` e fecha sozinho. Com
`SSHC_GUI_SMOKE_FORM=1` abre o formulário em vez de conectar.

## Layout local

```
~/Library/Application Support/sshc/
  sessions/                 # store: um .toml por sessão, pastas = grupos
    Cliente A/
      web-01.toml
  manifest-<hash>.json      # estado do sync DESTA máquina
  conflicts-<hash>/         # cópias que perderam conflito
```

O manifesto e os conflitos ficam **fora** da store de propósito: se ficassem
dentro, e a store fosse uma pasta do iCloud, o PC B herdaria o estado do PC A.
`hash` = SHA-256 do caminho da store, então duas stores não se misturam.

`SSHC_SESSIONS` sobrescreve a pasta da store; `SSHC_STATE_DIR` o estado.

## Regras do sync

| Situação | Ação |
|---|---|
| só local mudou | upload |
| só remoto mudou | download |
| os dois mudaram | **local vence**; versão remota vai para `conflicts-<hash>/` |
| apagou local | apaga no Drive |
| apagou no Drive | apaga local (se não houve edição depois do último sync) |
| mesmo nome em dois PCs | local vence (Drive não deduplica nome) |

Credenciais de servidor **nunca** entram na nuvem — a sessão guarda só metadado
(host, porta, usuário, emulação). Segredo vai para o Keychain (próxima etapa).

## Uso local

```bash
cargo run -- where                       # store e estado em uso
cargo run -- import <Sessions-do-SecureCRT> --dry-run
cargo run -- import <Sessions-do-SecureCRT>
cargo run -- list
```

O importador lê o `.ini` do SecureCRT (`S:`/`D:`/`Z:`/`B:`) preservando host,
porta, protocolo, usuário, emulação, autenticação e descrição.
`__FolderData__.ini` é metadado de pasta e é ignorado.

## Estrutura

- `crates/core` — modelo, store, importador SecureCRT
- `crates/sync` — PKCE, OAuth, Drive v3 (appDataFolder), reconciliação
- `crates/net` — transportes: Telnet (filtro IAC) e Serial
- `crates/term` — emulador VT sobre `vte` (cursor, erase, SGR 16/256/truecolor)
- `crates/cli` — binário `sshc` (onde, list, import, login, sync)
- `crates/gui` — app egui: árvore de sessões + painel de terminal

## Roadmap

1. **Agent SSH** (`SSH_AUTH_SOCK`) e passphrase de chave
2. **Abas / split** em vez de uma sessão por vez
3. **Scrollback** e busca no terminal
4. **Segurança**: Keychain por sessão/host; nada de senha em arquivo

## Licença

MIT.
