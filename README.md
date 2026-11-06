# sshc

Cliente SSH multi-sessão em Rust (protótipo). Marca provisória: `sshc`.

## Duas formas de manter as sessões na nuvem

**1. Pasta sincronizada (funciona já, zero configuração).**
A store é um diretório de arquivos `.toml`, um por sessão, espelhando a árvore
de pastas. iCloud/OneDrive/Google Drive/Dropbox já sincronizam pastas em
qualquer PC.

```
<root>/
  Cliente A/
    web-01.toml
```

**2. Login na conta (OAuth, sem depender do cliente de sync).**
`sshc login` abre o navegador, você autoriza, e o app grava as sessões num
**app-data folder privado** do seu Google Drive (`drive.appdata`): ele só enxerga
os arquivos que ele mesmo criou — o resto do Drive fica invisível. Noutro PC,
`sshc login` + `sshc sync` e as sessões descem.

O refresh token fica no **Keychain do macOS**, nunca em arquivo.

```bash
cargo run -- login --client-id <SEU_CLIENT_ID>   # guarda em ~/.sshc/client-id
cargo run -- sync --dry-run                      # mostra o que faria
cargo run -- sync                                # reconcilia
cargo run -- logout                              # apaga o token do Keychain
```

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

## Como o sync resolve conflito

| Situação | Ação |
|---|---|
| só local mudou | upload |
| só remoto mudou | download |
| os dois mudaram | **local vence**; versão remota vai para `.conflict/` |
| remoto sumiu | ignorado (delete não é propagado) |

Estado fica em `.sshc-sync.json` (não é sessão, não é sincronizado).

Credenciais de servidor **nunca** entram na nuvem — próxima etapa é o Keychain
por sessão.

## Uso local

```bash
cargo run -- where                       # providers detectados + store em uso
cargo run -- import <Sessions-do-SecureCRT> --dry-run
cargo run -- import <Sessions-do-SecureCRT>
cargo run -- list
```

A store é escolhida assim: `SSHC_SESSIONS` → primeiro provider de nuvem
detectado → `~/.sshc/sessions`.

O importador lê o `.ini` do SecureCRT (`S:`/`D:`/`Z:`/`B:`) preservando host,
porta, protocolo, usuário, emulação, autenticação e descrição.
`__FolderData__.ini` é metadado de pasta e é ignorado.

## Estrutura

- `crates/core` — modelo, store, detecção de pasta na nuvem, importador SecureCRT
- `crates/sync` — PKCE, OAuth, Drive v3 (appDataFolder), reconciliação
- `crates/cli` — binário `sshc`

## Roadmap

1. **Transporte**: `russh` (SSH) + telnet próprio + serial
2. **Emulação**: `vte` + `alacritty_terminal`
3. **UI**: janela com árvore de sessões, abas e painel de terminal
4. **Segurança**: Keychain por sessão/host; nada de senha em arquivo
5. **Backends**: OneDrive (Graph `Files.ReadWrite.AppFolder`) reaproveita o engine;
   Apple não tem OAuth equivalente — iCloud fica pela pasta `CloudDocs`

## Licença

MIT.
