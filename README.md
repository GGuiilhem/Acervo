<p align="center"><img src="brand/acervo-logo.svg" alt="Acervo" width="520"></p>

# Acervo

**Encontre, organize e controle seus arquivos.**

Aplicativo desktop para Windows construído em Rust e Tauri. O módulo atual oferece pesquisa rápida e progressiva por nome ou conteúdo, filtros avançados, substituição segura, ações em lote e compactação ZIP.

## Recursos

- Pesquisa paralela por nome ou conteúdo, sem bloquear a interface.
- Resultados exibidos à medida que são encontrados.
- Regex, palavra inteira, filtros por tipo, tamanho e data.
- Pesquisa e substituição com confirmação e backup opcional.
- Abrir, copiar, mover e compactar arquivos selecionados.
- Interface em português, com temas escuro e claro.
- Caminho rápido por bytes para pesquisas literais e paralelismo adaptado à CPU.
- Oito temas planos: Light, Dark, Slate, Nous, MidNight, Ember, Mono e CyberPunk.
- Interface em Português (Brasil) e English, com preferências persistentes.
- Notificação ao concluir pesquisas, inclusive em primeiro plano, com teste integrado nas configurações.
- Colunas redimensionáveis, com larguras persistentes e ordenação crescente/decrescente.
- Contagem prévia opcional com barra de progresso exata em toda a largura dos resultados.

## Desenvolvimento

Requisitos: Rust estável, Microsoft Visual C++ Build Tools e WebView2.

```powershell
cargo test
cargo build --release
```

O executável será gerado em `target/release/acervo.exe`.

Cada nova versão enviada para `main` é testada e compilada automaticamente pelo GitHub Actions. A automação cria uma Release com `Acervo-portable.exe` e `Acervo-setup.msi`; a versão em `Cargo.toml` e `tauri.conf.json` deve ser incrementada antes da publicação.

## Roadmap

- Organização e classificação de XMLs do eSocial.
- Presets e histórico de pesquisas.
- Renomeação e organização em lote.

## Licença

Ainda não definida.
