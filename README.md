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

- Fedora (testato su Fedora 44) con GTK 4.22, libadwaita 1.9 e WebKitGTK 6.0
  (2.52 o successivo).
- Rust 1.92 o successivo. `rusqlite` compila SQLite incluso nel crate, quindi non
  serve la libreria di sviluppo separata.
- Per l'importazione delle password serve `secret-tool` (pacchetto `libsecret`).

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
mkdir -p packaging/rpmbuild/SOURCES packaging/rpmbuild/SPECS
cp target/release/peats-browser packaging/SOURCES/* packaging/rpmbuild/SOURCES/
cp packaging/peats-browser.spec packaging/rpmbuild/SPECS/
rpmbuild --define "_topdir $PWD/packaging/rpmbuild" -bb packaging/rpmbuild/SPECS/peats-browser.spec
```

Il pacchetto esce in `packaging/rpmbuild/RPMS/x86_64/` e si installa con
`dnf install` o `rpm -i`. Serve WebKitGTK 6.0, GTK 4 e libadwaita di sistema.

## Test

```bash
cargo test          # 71 test unitari
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
src/main.rs         avvio, application ID, CSS
src/browser.rs      finestra, schede, spazi, vista affiancata, preferenze
src/model.rs        Settings, Session, segnalibri, cronologia
src/url.rs          risoluzione di indirizzi e ricerche
src/storage.rs      SQLite (cronologia FTS5, credenziali) e JSON
src/filters.rs      blocco contenuti e filtri cosmetici
src/suggest.rs      suggerimenti della barra indirizzi
src/import.rs       importazione da altri browser
src/crypto.rs       ChaCha20 per le password
src/keyring.rs      chiave nel portachiavi di sistema
src/update.rs       release GitHub, versioni, SHA-256
src/style.css       stile dell'interfaccia
packaging/          spec RPM e asset .desktop/.svg
```

## Licenza

MIT.
