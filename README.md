# Peats

Browser leggero per Fedora, scritto in Rust su GTK4, libadwaita e WebKitGTK 6.0.
Una finestra sola, niente telemetria, dati solo in locale.

## Funzioni

- **Spazi di lavoro isolati**: ogni spazio usa un profilo WebKit separato (cookie,
  cache, archiviazione locale distinti); schede raggruppabili e fissabili.
- **Vista affiancata** con due schede affiancate nella stessa finestra.
- **Finestra privata** (`Ctrl+Alt+P`): sessione effimera, nessuna voce in
  cronologia, nessun download registrato, nessun salvataggio della sessione.
- **Blocco contenuti** nativo: regole di rete più 21 selettori cosmetici, con
  eccezioni per dominio; le regole compilate vengono riusate fra gli avvii.
- **Privacy**: blocco dei cookie di terze parti, segnali GPC/DNT iniettati nelle
  pagine, assistenza ai banner dei consensi (opzionale), gestione permessi per
  sito (geolocalizzazione, notifiche, fotocamera, microfono…).
- **Cronologia** su SQLite con indice FTS5, potatura a giorni configurabile e
  scritture raggruppate.
- **Suggerimenti nella barra indirizzi**: schede aperte, cronologia, segnalibri e
  ricerca rapida; motori di ricerca personalizzati con parola chiave (`g rust`).
- **Importazione dati** da Chrome, Firefox, HTML (formato Netscape), CSV e JSON:
  cronologia, segnalibri e credenziali salvate sul dispositivo.
- **Account e password** importati cifrati con ChaCha20 (nonce casuale per voce),
  con la chiave a 256 bit custodita nel portachiavi di sistema.
- **Aggiornamenti** dalle release del repository, con verifica SHA-256 del
  pacchetto prima dell'installazione.
- Modalità lettura, zoom per sito, ripristino della sessione, schede inattive
  scaricabili per liberare memoria, supporto minimo alle estensioni.

## Requisiti

Linux (build principale):

- Fedora (testato su Fedora 44) con GTK 4.22, libadwaita 1.9 e WebKitGTK 6.0
  (2.52 o successivo).
- Rust 1.92 o successivo. `rusqlite` compila SQLite incluso nel crate, quindi non
  serve la libreria di sviluppo separata.
- Per l'importazione delle password serve `secret-tool` (pacchetto `libsecret`).

Windows (port in preparazione):

- Windows 10/11 con WebView2 Runtime (presente di serie su Windows 11).
- Rust stabile e i Build Tools di Visual Studio con il carico C++.
- Nessuna dipendenza GTK: il motore è WebView2 tramite `wry`.

## Compilare ed eseguire

```bash
cargo build --release
./target/release/peats-browser
```

I dati stanno nelle cartelle XDG dell'utente (`~/.local/share/io.github.peats.Browser`,
`~/.config/io.github.peats.Browser`, `~/.cache/io.github.peats.Browser`).

## Creare il pacchetto RPM

Lo spec non ricompila: prende il binario già compilato e gli asset da
`packaging/SOURCES`. Da un clone pulito:

```bash
cargo build --release
mkdir -p packaging/rpmbuild dist
install -m0755 target/release/peats-browser packaging/SOURCES/peats-browser
rpmbuild -bb packaging/peats-browser.spec \
    --define "_topdir $PWD/packaging/rpmbuild" \
    --define "_sourcedir $PWD/packaging/SOURCES" \
    --define "_rpmdir $PWD/dist" \
    --define "_rpmfilename %{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}.rpm" \
    --define "debug_package %{nil}"
```

Il pacchetto esce in `dist/peats-browser-0.1.0-1.fc44.x86_64.rpm` e si installa
con `dnf install` o `rpm -i`. Serve WebKitGTK 6.0, GTK 4 e libadwaita di sistema.

## Peats per Windows

WebKitGTK non esiste su Windows, quindi la variante Windows usa **WebView2**
(Edge/Chromium) al suo posto: due WebView, la barra dell'indirizzo sopra e la
pagina sotto. Il resto — impostazioni, cronologia SQLite, risoluzione degli
indirizzi, importazione, aggiornamenti — arriva dal crate condiviso `peats-core`.

```bash
cargo build --release -p peats-windows
.\target\release\peats-windows.exe
```

Stato del port: funzionano navigazione, indietro/avanti, ricarica, barra
collegata all'indirizzo corrente e registrazione della cronologia. Mancano
schede multiple, spazi di lavoro, preferenze, segnalibri, importazione dalla GUI
e l'aggiornamento automatico specifico per Windows: sono i passaggi successivi.
La compilazione è verificata dal workflow `.github/workflows/windows.yml`; la
build non è mai stata eseguita su una macchina Windows reale durante lo
sviluppo, quindi il primo avvio va provato lì.

### Pacchetto MSI

L'eseguibile si distribuisce come MSI, che è anche quello che Peats scarica
per aggiornarsi su Windows (lo installa con `msiexec /i`):

```bash
cargo xwin build --release -p peats-windows --target x86_64-pc-windows-msvc \
    --cross-compiler clang --msvc-sysroot ~/.cache/windows-msvc-sysroot/windows-msvc-sysroot
packaging/windows/build-msi.sh
# -> dist/peats-browser-0.1.0-x64.msi
```

Lo script sostituisce versione e percorso dell'eseguibile in
`packaging/windows/peats.wxs.in` e compila con `wixl` (pacchetto `msitools`), che
funziona anche su Linux: nessun toolset WiX di Microsoft. L'MSI installa in
`C:\Program Files\Peats` (come `peats-browser.exe`), aggiunge «Peats Browser» al
menu Start e condivide lo stesso `UpgradeCode` fra le versioni con
`MajorUpgrade`, così un nuovo MSI aggiorna l'installazione esistente invece di
affiancarne una seconda. Serve il WebView2 Runtime (di serie su Windows 11).

## Release

Allegati della versione 0.1.0 su GitHub Releases:

| File | A cosa serve |
| --- | --- |
| `peats-browser-0.1.0-1.fc44.x86_64.rpm` | Linux Fedora, si installa con `dnf` |
| `peats-browser-0.1.0-x64.msi` | Windows: installazione e aggiornamenti |
| `peats-browser-0.1.0-portable.exe` | Windows: prova senza installare |
| `SHA256SUMS` | impronte SHA-256 dei tre file |

Il tag della release deve essere il numero di versione (`0.1.0` o `v0.1.0`):
Peats lo confronta numericamente con quello installato. Fra gli allegati Windows
la ricerca sceglie l'MSI (il nome contiene `x64`, il portable no) e su Linux il
RPM `x86_64`. La descrizione della release può riportare la riga
`sha256: <impronta>` come ripiego: l'app usa di norma il `digest` che GitHub
calcola per ogni allegato.

## Test

```bash
cargo test                  # 71 test (build Linux + crate condiviso)
cargo test -p peats-core    # test del solo crate condiviso
cargo clippy --all-targets
```

I test coprono risoluzione di URL e ricerche, modelli delle impostazioni,
persistenza SQLite e cronologia FTS, filtri cosmetici, parser di importazione,
cifratura ChaCha20 (vettore della RFC 8439) e confronto/parsing degli
aggiornamenti.

## Aggiornamenti

Nelle Preferenze il campo «Repository GitHub o manifest» è precompilato con
`https://github.com/navreet2317/Peats-browser`. Una volta al giorno (e su
richiesta con «Controlla adesso») Peats legge
`https://api.github.com/repos/navreet2317/Peats-browser/releases/latest`, usa il
tag come versione e l'allegato `.rpm` come pacchetto. In alternativa si può
indicare un manifest JSON:

```json
{"version": "0.2.0",
 "url": "https://esempio.test/peats-browser-0.2.0-1.fc44.x86_64.rpm",
 "sha256": "…",
 "notes": "Novità di questa versione"}
```

Il pacchetto viene scaricato in cache, confrontato con lo SHA-256 (dal `digest`
dell'allegato GitHub o da una riga `sha256: …` nella descrizione della release)
e solo allora passato a `dnf install` tramite polkit, che chiede la password di
amministratore. Senza hash l'installazione viene annullata. Una build di
sviluppo (non installata in `/usr/bin`) non si aggiorna da sola.

## Importare i dati

Preferenze → «Importa dati…». Il formato è riconosciuto dal contenuto:

- database SQLite di Chrome/Chromium (`History`, `Bookmarks`) e Firefox
  (`places.sqlite`);
- file HTML esportato dai browser (formato Netscape);
- CSV con intestazione `url,title[,visited_at]` oppure
  `url,username,password[,notes]`;
- JSON con lo stesso contenuto.

Il file `Login Data` di Chrome non è leggibile (le password sono cifrate con una
chiave del profilo): per quelle serve l'esportazione CSV.

## Limiti noti

- Le password importate si possono copiare negli appunti da Preferenze → «Account
  e password», ma non vengono compilate automaticamente nei moduli: WebKitGTK non
  espone un'API per aggiungere credenziali al suo archivio.
- L'aggiornamento automatico richiede una release pubblicata con un pacchetto
  `.rpm` allegato.
- Le finestre sono native GTK: non esiste un'integrazione di test end-to-end.

## Struttura

```
src/main.rs           avvio, application ID, CSS
src/browser.rs        finestra, schede, spazi, vista affiancata, preferenze
src/style.css         stile dell'interfaccia GTK

peats-core/src/model.rs      Settings, Session, segnalibri, cronologia
peats-core/src/url.rs        risoluzione di indirizzi e ricerche
peats-core/src/storage.rs    SQLite (cronologia FTS5, credenziali) e JSON
peats-core/src/filters.rs    blocco contenuti e filtri cosmetici
peats-core/src/suggest.rs    suggerimenti della barra indirizzi
peats-core/src/import.rs     importazione da altri browser
peats-core/src/crypto.rs     ChaCha20 per le password
peats-core/src/keyring.rs    chiave nel portachiavi (secret-tool / DPAPI)
peats-core/src/update.rs     release GitHub, versioni, SHA-256

windows/src/main.rs   barra e pagina WebView2, comandi e cronologia
windows/ui/chrome.html  interfaccia della barra su Windows
packaging/            spec RPM e asset .desktop/.svg
```

## Licenza

MIT.
