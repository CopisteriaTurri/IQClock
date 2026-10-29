# IQClock

## CI e release

- Sulle PR gira solo `.github/workflows/pr.yml` (clippy + `cargo test`, solo Linux, saltata se cambiano solo `.md`). Windows e build release non vengono provati sulle PR.
- La CI completa (`.github/workflows/ci.yml`) **non** gira sui push a `main`: parte solo quando si pusha un tag `v*`, per non sprecare minuti di Actions.
- Un tag = una release: clippy, test e build su Linux e Windows, audit RustSec, poi il job `release` crea la release GitHub con `iqclock-linux-x64` e `iqclock-win-x64.exe`.
- Per rilasciare: `git tag vX.Y.Z && git push origin vX.Y.Z`. Non pushare tag `v*` se non per fare una release.
- I push diretti a `main` non sono verificati: prima di pushare lancia in locale `cargo clippy --all-targets -- -D warnings` e `cargo test`.
- Dependabot (`.github/dependabot.yml`) è mensile; le sue PR passano dal workflow `pr.yml`.
- I binari in release includono pdfium (feature `pdfium-incluso`, estratto nella cache utente al primo avvio): la CI lancia `scarica-pdfium.sh` prima di compilare (cartella `pdfium/`, ignorata da git). Senza `pdfium/` la feature non compila.
