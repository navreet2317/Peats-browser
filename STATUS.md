# Peats Browser — stato del progetto

## Architettura

Applicazione GTK4/libadwaita in Rust su WebKitGTK 2.54 (crate `webkit6` 0.6.1,
feature `v2_52`). Un solo processo applicativo, rendering in un processo
WebKit separato.

| Modulo | Responsabilità |
| --- | --- |
| `src/main.rs` | Applicazione, CSS globale, avvio con gestione errori |
| `src/browser.rs` | Finestra, schede, spazi, dialoghi, azioni, privacy |
| `src/model.rs` | Tipi serializzati (impostazioni, sessione, gruppi) |
| `src/storage.rs` | SQLite + JSON, percorsi XDG, eccezioni e permessi |
| `src/url.rs` | Risoluzione dell'input della barra indirizzi |
| `src/blocklist.rs` | Lista incorporata di host pubblicitari (fallback) |
| `src/filters.rs` | Liste hosts → regole WebKit, cache e metadati |
| `src/privacy.rs` | Segnali GPC/DNT via JavaScript |
| `src/consent.rs` | Assistenza ai banner dei CMP riconosciuti |
| `src/reader.rs`, `src/extensions.rs` | Modalità lettore, estensioni |
| `src/style.css` | Stile dei widget GTK |

### Fonti di verità

- Schede/spazi: stato in memoria, `session.json` a ogni modifica e alla chiusura.
- Preferenze: `settings.json` (blocco contenuti, cookie di terze parti,
  segnali di privacy, gestione banner).
- Cronologia, segnalibri, download, zoom, permessi, **eccezioni al blocco**:
  SQLite in `<data>/peats-browser/peats.sqlite3`.
- Liste scaricate e relativa compilazione:
  `<data>/peats-browser/content-filters/`.
- Cookie/cache/localStorage: `WebKitWebsiteDataManager`.

## Blocco pubblicità e tracker

Il filtro usa il meccanismo nativo di WebKit: `UserContentFilterStore` compila
un array JSON di regole `trigger`/`action`, il filtro viene aggiunto al
`UserContentManager` **condiviso da tutte le WebView** (prima non era collegato
affatto: le regole venivano compilate ma non applicate).

- **Lista incorporata**: `src/blocklist.rs`, 37 reti pubblicitarie, usata come
  fallback e sempre unita alla lista scaricata.
- **Lista pubblica**: [StevenBlack/hosts](https://github.com/StevenBlack/hosts),
  licenza MIT, formato hosts (`0.0.0.0 dominio`). Il file viene scaricato via
  HTTP (soup 3), convertito in regole e salvato con metadati (`peats-meta.json`:
  fonte, URL, licenza, data, numero di host).
- **Aggiornamento**: pulsante «Aggiorna liste» nel popover Privacy, oppure
  automatico ~4 s dopo l'avvio quando la copia ha più di 7 giorni. In caso di
  errore di rete o lista non valida resta la copia precedente e compare un
  toast; il conteggio è visibile nel popover («StevenBlack/hosts (MIT): N host,
  aggiornata il …»).
- **Tipi di risorsa**: vengono bloccati image, style-sheet, script, font, raw,
  svg-document, media, popup, ping, fetch, websocket e other. `document` è
  escluso: WebKit non distingue il documento principale da un iframe, quindi
  bloccare `document` impedirebbe anche la navigazione diretta verso l'host. Gli
  iframe di host bloccati restano caricabili come documento, ma le loro
  sottorisorse vengono bloccate.
- **Eccezioni per sito**: tabella `block_exceptions` (origine con porta).
  Vengono tradotte in regole `ignore-previous-rules` con `if-domain`, quindi
  valgono per quel dominio e i sottodomini senza disattivare il blocco altrove.
  Si attivano dal popover Privacy («Su questo sito»), dalla voce di menu
  «Blocco contenuti su questo sito» o dal dialogo «Eccezioni»; sono revocabili
  singolarmente o tutte insieme.
- **Conteggi**: WebKit non espone il numero di richieste bloccate; il popover
  mostra il numero di host in lista e di eccezioni, non un conteggio di blocchi.
- **Pop-up**: le finestre verso host in lista non vengono aperte
  (`create` → nessuna scheda); quelle verso altri host si aprono in una scheda.

## Protezione antitracciamento e cookie

- **ITP**: `NetworkSession::set_itp_enabled(true)` all'avvio (API disponibile
  in WebKitGTK 6).
- **Cookie di terze parti**: `CookieManager::set_accept_policy(NoThirdParty)`
  quando l'opzione è attiva (default), `Always` quando l'utente la disattiva.
  La politica dei cookie di WebKit è **globale**: la versione in uso non offre
  eccezioni per singolo sito, quindi non sono implementate.
- **Segnali GPC/DNT**: iniettati come `navigator.globalPrivacyControl` e
  `navigator.doNotTrack` con uno script all'avvio del documento. `webkit6`
  **non espone alcuna API per aggiungere header HTTP** (`NetworkSession`,
  `WebView`, `WebContext` non hanno setter di header): i server che leggono
  `Sec-GPC`/`DNT` dagli header non li ricevono. È quindi presentato come segnale
  informativo per le pagine, non come blocco.
- **Anti-fingerprinting**: WebKitGTK 2.54 non offre un'API dedicata e non sono
  stati introdotti script che alterano Canvas/WebGL/audio: nessuna mitigazione
  dichiarata.

## Banner di consenso

Funzione opzionale (spenta di default). Clicca **solo** il pulsante di rifiuto
previsto da tre CMP, tramite selettore CSS fisso:

| CMP | Selettore |
| --- | --- |
| OneTrust | `#onetrust-reject-all-handler` |
| Cookiebot | `#CybotCookiebotDialogBodyButtonDecline` |
| Didomi | `.didomi-continue-without-agreeing` |

Lo script non cerca pulsanti per testo o aspetto, non interpreta il banner, non
invia moduli e non preme mai «Accetta»; se il banner non è riconosciuto la
pagina resta com'è. La copertura è verificata solo su pagine locali che
riproducono quegli identificatori: non è una verifica sui siti reali.

## Barra indirizzi con suggerimenti

Sotto la barra indirizzi si apre un elenco aggiornato mentre si digita
(`src/suggest.rs` per la logica, `Browser::update_suggestions` per la UI):

1. **Schede aperte** nello spazio corrente, filtrate per titolo o indirizzo:
   sceglierle porta la scheda in primo piano.
2. **Segnalibri e cronologia**, interrogati con `LIKE` su SQLite
   (`Storage::search_bookmarks`, `Storage::search_history`), raggruppati per URL
   e senza duplicati rispetto alle schede già mostrate.
3. **Ricerca rapida DuckDuckGo**: dopo 250 ms di pausa nell'ultima digitazione
   parte una richiesta a `https://duckduckgo.com/ac/?q=…&type=list` su una
   sessione HTTP dedicata, senza cookie del browser e senza referrer. Le
   risposte arrivate dopo un cambio di testo vengono scartate (contatore di
   generazione). Il testo che sembra un indirizzo non interroga il servizio.

La voce «Cerca …» finale porta sempre al motore predefinito. Frecce su/giù,
Invio, clic ed Esc funzionano dalla barra e dall'elenco: il popover e le righe
non sono focalizzabili, quindi il fuoco resta nella barra finché non si sceglie
una voce. Dopo la scelta il popover si chiude e non può riaprirsi da solo
(generazione dei suggerimenti invalidata e aggiornamenti programmatici della
barra soppressi); con Invio o le frecce il fuoco torna alla barra indirizzi con
l'indirizzo aggiornato, con il clic del mouse passa alla pagina WebView per
scorrere o digitare subito. L'invio di rete è
disattivabile dalle Preferenze («Suggerimenti nella barra indirizzi»): con
l'opzione spenta restano schede, segnalibri, cronologia e ricerca diretta.

## Spazi di lavoro isolati

Ogni spazio ha un **profilo dati** persistente in `session.json`
(`WorkspaceSession::profile`). Al primo avvio dopo questa versione il primo
spazio conserva il profilo storico (`<data>/webkit`, quindi nessun cookie
perso) e ogni spazio nuovo riceve un identificatore `ws-…`:

- sessione di rete dedicata (`NetworkSession` in
  `Browser::workspace_sessions`), con cartelle
  `<data>/webkit-profiles/<profilo>` e `<cache>/webkit-profiles/<profilo>`;
- cookie, localStorage, IndexedDB, credenziali e cache restano separati fra
  «Lavoro» e «Personale» nella stessa finestra;
- ITP e politica dei cookie vengono applicati a tutte le sessioni, e la
  cancellazione dati dalle Preferenze agisce su tutte le sessioni;
- eliminare uno spazio non cancella i dati su disco (restano nella sua
  cartella), così un errore non distrugge sessioni di lavoro.

## Vista affiancata

Il contenuto della finestra è un `gtk::Paned` con due `gtk::Stack`: a sinistra
la scheda attiva, a destra una seconda scheda dello stesso spazio con la
propria WebView. Comandi: «Vista affiancata» (`Ctrl+Shift+S`) e «Inverti
riquadri» (`Ctrl+Shift+X`), dal menu o dal riquadro. La barra indirizzi, i
pulsanti di navigazione e lo zoom seguono il riquadro che ha il fuoco; attivare
dalla lista la scheda di destra chiude lo split riportandola nel riquadro
principale, e chiudere la scheda di destra chiude lo split. La vista affiancata
non viene salvata in `session.json`.

## Finestra privata

`Ctrl+Alt+P` (o menu → «Nuova finestra privata») apre un secondo `Browser`
indipendente con:

- sessioni **effimere** (`NetworkSession::new_ephemeral`) per la finestra e per
  ogni suo spazio: cookie, cache, localStorage e credenziali non finiscono su
  disco e spariscono alla chiusura;
- salvataggio della sessione disattivato, così le schede aperte non vengono
  registrate in `session.json`;
- cronologia non scritta e download non registrati nel database condiviso (il
  file scaricato resta su disco, come in ogni browser);
- titolo della finestra con la marca «privata».

Le impostazioni restano globali (sono preferenze dell'app, non dati di
navigazione). L'elenco `EXTRA_WINDOWS` tiene vivo l'`Rc` della finestra finché
questa esiste e lo libera alla chiusura.

## Filtro contenuti riusato fra gli avvii

`filters::fingerprint` calcola l'impronta FNV-1a delle regole generate e
`peats-compiled.json` la memorizza accanto alla lista. All'avvio, se l'impronta
corrisponde, il filtro viene caricato con `UserContentFilterStore::load`
(senza ricompilare); solo se manca o non è più caricabile viene ricompilato e
salvato, aggiornando l'impronta.

## Blocco cosmetico

Oltre alle richieste, il filtro contiene regole `css-display-none` per 21
selettori conservativi (`filters::COSMETIC_SELECTORS`, scritti nel progetto,
nessuna lista di terze parti copiata): spariscono i contenitori vuoti lasciati
dagli annunci. Le eccezioni per sito valgono anche qui tramite `unless-domain`,
e l'opzione «Nascondi gli spazi pubblicitari» nelle Preferenze spegne solo
questa parte (le richieste restano bloccate).

## Cronologia: indice FTS5, potatura, scritture raggruppate

- Tabella virtuale `history_fts` con triggers su inserimento, modifica ed
  eliminazione; per i database esistenti l'indice viene ricostruito una volta
  (`rebuild`) all'apertura.
- `search_history` interroga prima l'indice (prefissi: «rus» trova «Rust» e
  «Rustico») e ripiega su `LIKE` per le sottostringhe a metà parola.
- `prune_history` elimina le visite più vecchie del limite scelto (Preferenze →
  Prestazioni → «Conserva la cronologia», 0 = nessun limite) e viene eseguita
  all'avvio e al cambio dell'impostazione.
- Le visite vengono accodate e scritte con `add_history_many` in un'unica
  transazione dopo 2 secondi (o alla chiusura della finestra), invece di una
  `INSERT` per caricamento; `PRAGMA synchronous = NORMAL` riduce gli fsync.

## Schede inattive

Opzione «Scarica le schede inattive» (spenta di default): ogni minuto le
WebView non visibili da più di 10 minuti vengono rimosse e la scheda conserva
URL, titolo e `pending_uri`, quindi si ricarica alla prima apertura. Testo non
inviato nei moduli va perso: per questo è opt-in e non tocca mai la scheda
attiva né quella del riquadro affiancato.

## Importazione di dati, cronologia e account

Menu → «Importa dati…» (anche in Impostazioni → Dati) legge un file scelto dal
disco. Il formato si riconosce dal contenuto, non dal nome:

- **Database SQLite**: `History` di Chrome/Edge (tabella `urls`) e
  `places.sqlite` di Firefox (`moz_places`, `moz_bookmarks`). Le date originali
  vengono conservate (microsecondi dal 1601 per Chrome, dall'epoca Unix per
  Firefox); il file viene copiato in `/tmp` solo per la lettura e poi cancellato.
- **Segnalibri HTML** esportati (formato Netscape `<A HREF=…>`), con entità
  `&amp;` ecc. decodificate.
- **CSV**: intestazione `url,title[,visited_at]` per la cronologia oppure
  `url,username,password` (riconosciuti i nomi di colonna di Chrome e Firefox);
  virgolette, virgole e righe spezzate dentro i campi sono gestite.
- **JSON**: lista di oggetti con `url` + `password` (account), `url` + `title`
  (cronologia) o solo `url` (segnalibro).

La cronologia importata passa dall'indice FTS5 e salta i duplicati esatti; i
segnalibri aggiornano il titolo se l'URL è già presente.

### Password e account sul dispositivo

- Tabella `credentials` in `peats.db`: host, URL, nome utente, nonce e segreto
  **cifrati con ChaCha20** (`src/crypto.rs`, verificato con il vettore di test
  RFC 8439). Una voce per coppia host + nome utente.
- La chiave a 256 bit sta nel portachiavi di sistema (`secret-tool`, servizio
  `io.github.peats.Browser`, attributo `purpose=passwords`), non nel database:
  copiare `peats.db` non basta per leggere le password. Se il portachiavi non è
  disponibile l'importazione delle password viene rifiutata invece di salvarle
  in chiaro.
- Finestra «Account e password» (menu o Impostazioni → Dati): elenco per host e
  nome utente, copia della password negli appunti (mai mostrata a schermo),
  eliminazione della singola voce o di tutte.
- Le password salvate dai profili Chrome/Edge sono cifrate dal sistema e non si
  possono leggere: servono esportate in CSV, e il messaggio di import lo dice.

### Limiti

- Le password importate **non entrano nel riempimento automatico**: WebKitGTK
  espone solo l'attivazione dell'archivio credenziali interno
  (`persistent_credential_storage_enabled`), non un'API per aggiungerne. In
  Peats si copiano dalla finestra «Account e password».
- Nessuna sincronizzazione e nessun invio in rete: i dati restano nel database
  locale e nel portachiavi.
- La cronologia importata più vecchia del limite di conservazione viene potata
  al prossimo avvio, come le visite normali.

## Aggiornamenti

Impostazioni → Aggiornamenti: interruttore «Controlla gli aggiornamenti»,
campo per l'indirizzo HTTPS del manifest e pulsante «Controlla adesso».

```json
{"version": "0.1.1",
 "url": "https://esempio.test/peats-browser-0.1.1-1.fc44.x86_64.rpm",
 "sha256": "…",
 "notes": "Novità di questa versione"}
```

Flusso (`src/update.rs`, `Browser::maybe_check_updates`):

1. all'avvio, una volta al giorno e solo con l'opzione attiva (mai nelle
   finestre private), `GET` del manifest su una sessione HTTP dedicata;
2. `is_newer` confronta le versioni per segmenti numerici (`0.1.10` è più
   recente di `0.1.9`);
3. se la versione è più recente compare una finestra con le note e i pulsanti
   «Più tardi» / «Installa»;
4. «Installa» scarica il pacchetto, ne calcola lo SHA-256 e lo confronta con
   quello del manifest: se non coincide l'installazione viene annullata;
5. solo per installazioni di sistema (`/usr/bin`, `/usr/local/bin`) il file
   finisce in `<cache>/updates/` e viene passato a `pkexec dnf install -y`:
   la password di amministratore la chiede polkit, l'app non la vede né la
   conserva.

Le build di sviluppo (`target/release/peats-browser`) rifiutano
l'autoinstallazione. Il manifest è l'unico punto di rete: senza indirizzo e
senza opzione attiva non parte nessuna richiesta.

## Aspetto

`src/style.css` è un piccolo sistema di stile costruito sui colori di sistema
(`@window_bg_color`, `@window_fg_color`, `@accent_bg_color`): le superfici sono
`alpha(@window_fg_color, …)` e l'accento è sempre quello scelto dall'utente,
quindi chiaro e scuro funzionano senza duplicare le regole.

- barra indirizzi a pillola, con alone colorato al fuoco;
- schede con barra di accento arrotondata, hover e selezione animati
  (120–160 ms), pulsanti di navigazione circolari;
- tile delle schede fissate con ombra all'hover, suggerimenti con chip per tipo
  e popover con animazione d'ingresso breve;
- barra di caricamento con gradiente d'accento.

La pagina iniziale (`start_page_html`) è ridisegnata: alone animato a due
macchie (22–26 s), scheda in vetro con bordo che si illumina mentre si scrive,
quattro scorciatoie (Wikipedia, GitHub, DuckDuckGo, YouTube), saluto legato
all'ora del sistema e promemoria delle scorciatoie da tastiera. Rispetta
`prefers-reduced-motion` disattivando animazioni e transizioni. Il modello di
ricerca resta lo stesso JSON passato dal browser: nessuna logica nuova lato
pagina.

## Pacchetto

`packaging/peats-browser.spec` crea un RPM con il binario release, la voce di
menu (`data/io.github.peats.Browser.desktop`) e l'icona
(`data/io.github.peats.Browser.svg`). Dipendenze dichiarate: `webkitgtk6.0`,
`gtk4`, `libadwaita`.

## Verifiche

- `cargo build`: OK, nessun warning.
- `cargo test`: 66 test (confronto delle versioni, SHA-256 con vettori noti,
  parsing del manifest degli aggiornamenti, URL, modelli, sessioni, SQLite, ricerche FTS5 e
  fallback `LIKE`, potatura della cronologia, scritture in blocco, permessi,
  eccezioni, parsing hosts, regole con eccezioni, regole cosmetiche e loro
  `unless-domain`, impronte del filtro, metadati compilati, cache, script dei
  segnali e dei banner, suggerimenti, riconoscimento degli indirizzi,
  importazione da SQLite di Chrome, CSV di cronologia e password, HTML dei
  segnalibri, JSON, cifratura ChaCha20 con vettore RFC 8439, account
  importati in SQLite).
- `cargo clippy --all-targets`: nessun errore; restano avvisi di stile.
- Prova end-to-end con server locale: lista hosts scaricata (121 host),
  compilata da WebKit; pagina con immagine su host in lista → immagine bloccata
  (nessuna richiesta al server, `img=blocked`); con eccezione per l'origine →
  immagine caricata; eccezione rimossa dall'azione dell'app → blocco riattivato
  e scheda ricaricata; segnali GPC/DNT letti dalla pagina (`gpc=1 dnt=1`,
  `gpc=0 dnt=undefined` con l'opzione spenta); banner di prova OneTrust
  cliccato solo con la funzione attiva, pulsante «Accetta» mai toccato.

## Limiti noti

- Nessun conteggio di richieste bloccate (il motore non lo espone).
- Gli iframe di host bloccati restano caricabili come documento.
- Cookie di terze parti: politica globale, nessuna eccezione per sito.
- GPC/DNT non sono header HTTP.
- Nessuna mitigazione anti-fingerprinting.
- Banner: solo tre CMP, verificati su pagine di prova.
- Un solo profilo applicativo (le cache sono separate per spazio, non i
  segnalibri né la cronologia): schede, segnalibri e cronologia sono comuni a
  tutti gli spazi.
- I suggerimenti di ricerca richiedono la rete; senza rete restano le fonti
  locali.
- La vista affiancata non sopravvive al riavvio e non ha una terza colonna.
- `session.json` riscritto a ogni cambio scheda; nessun riordino delle schede
  via trascinamento; estensioni senza versionamento.

## Verifica di questa revisione

Compilazione (`cargo build`, `cargo build --release`) e test automatici
eseguiti; **l'app non è stata avviata** per provare a mano le tre funzioni in
questa revisione, come richiesto.
