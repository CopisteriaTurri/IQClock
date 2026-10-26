# HP Clock

## CI e release

- La CI (`.github/workflows/ci.yml`) **non** gira sui push a `main` né sulle PR: parte solo quando si pusha un tag `v*`, per non sprecare minuti di Actions.
- Un tag = una release: clippy, test e build su Linux e Windows, audit RustSec, poi il job `release` crea la release GitHub con `hpclock-linux-x64` e `hpclock-win-x64.exe`.
- Per rilasciare: `git tag vX.Y.Z && git push origin vX.Y.Z`. Non pushare tag `v*` se non per fare una release.
- Visto che `main` non è verificata dalla CI, prima di mergiare/pushare lancia in locale `cargo clippy --all-targets -- -D warnings` e `cargo test`.
- Dependabot (`.github/dependabot.yml`) è mensile; le sue PR non hanno controlli automatici, vanno provate a mano.
- I binari in release non includono pdfium: serve `scarica-pdfium.sh` (cartella `pdfium/`, ignorata da git).
