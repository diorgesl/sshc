# sshc

Cliente SSH multi-sessão em Rust (protótipo). Marca provisória: `sshc`.

## Decisão central: sessões são arquivos

Não existe banco nem servidor de sincronização. A store é um diretório de
arquivos `.toml`, **um por sessão**, espelhando a árvore de pastas:

```
<root>/
  Cliente A/
    web-01.toml
    db-01.toml
```

Por que: iCloud Drive, OneDrive, Google Drive e Dropbox já sincronizam pastas em
qualquer PC. Zero OAuth, zero API, zero SDK. Conflito deixa de ser "um banco
inteiro" e vira "um arquivo". Escrita é atômica (tmp + rename) para o sync nunca
ver um `.toml` pela metade.

Credenciais **nunca** entram aqui — próxima etapa é o Keychain do macOS.

## Uso

```bash
cargo run -- where                       # providers detectados + store em uso
cargo run -- import <Sessions-do-SecureCRT> --dry-run
cargo run -- import <Sessions-do-SecureCRT>
cargo run -- list
```

A store é escolhida assim, nesta ordem:

1. `SSHC_SESSIONS=/caminho` (variável de ambiente)
2. primeiro provider de nuvem detectado (iCloud, Google Drive, OneDrive, Dropbox)
3. `~/.sshc/sessions`

O importador lê o formato `.ini` do SecureCRT (`S:`/`D:`/`Z:`/`B:`) e
preserva host, porta, protocolo, usuário, emulação, autenticação e descrição.
`__FolderData__.ini` é metadado de pasta e é ignorado.

## Estrutura

- `crates/core` — modelo, store, detecção de nuvem, importador SecureCRT
- `crates/cli` — binário `sshc`

## Roadmap

1. **Transporte**: `russh` (SSH) + telnet próprio + serial
2. **Emulação**: `vte` + `alacritty_terminal`
3. **UI**: janela com árvore de sessões, abas e painel de terminal
4. **Segurança**: Keychain por sessão/host; nada de senha em arquivo
5. **Sync**: opcionalmente, revisão/LWW por sessão + backup versionado do diretório

## Licença

MIT.
