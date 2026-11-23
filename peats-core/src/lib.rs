//! Moduli di Peats che non dipendono dall'interfaccia: modelli dati,
//! risoluzione degli indirizzi, SQLite, importazione, filtri, cifratura e
//! aggiornamenti.
//!
//! Sono condivisi fra la build Linux (`peats-browser`, GTK4 + WebKitGTK) e
//! quella Windows (`peats-windows`, WebView2): l'unica parte che cambia è il
//! front-end. Le funzioni che parlano con il sistema operativo (portachiavi e
//! installazione degli aggiornamenti) hanno un'implementazione per piattaforma.

pub mod blocklist;
pub mod consent;
pub mod crypto;
pub mod extensions;
pub mod filters;
pub mod import;
pub mod keyring;
pub mod model;
pub mod privacy;
pub mod reader;
pub mod storage;
pub mod suggest;
pub mod update;
pub mod url;
