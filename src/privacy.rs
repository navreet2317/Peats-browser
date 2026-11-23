//! Segnali di privacy esposti alle pagine.
//!
//! WebKitGTK 2.54 non espone un'API per aggiungere header HTTP alle richieste
//! (in `webkit6` non esiste un setter di header per `NetworkSession` o
//! `WebView`). Questi segnali sono quindi visibili solo da JavaScript, tramite
//! le proprietà lette dagli script delle pagine: i server che leggono
//! `Sec-GPC` o `DNT` dagli header **non** li ricevono. Vengono presentati come
//! segnale informativo, non come blocco.

/// Script iniettato all'avvio del documento: imposta `navigator.globalPrivacyControl`
/// e `navigator.doNotTrack`. Non tocca nulla se la pagina non è scriptabile.
pub fn signals_script() -> String {
    r#"(function () {
  try {
    Object.defineProperty(Navigator.prototype, "globalPrivacyControl", {
      get: function () { return true; },
      configurable: true
    });
  } catch (error) {}
  try {
    Object.defineProperty(Navigator.prototype, "doNotTrack", {
      get: function () { return "1"; },
      configurable: true
    });
  } catch (error) {}
})();"#
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_exposes_both_signals() {
        let script = signals_script();
        assert!(script.contains("globalPrivacyControl"));
        assert!(script.contains("doNotTrack"));
        assert!(script.contains("configurable: true"));
        // Lo script non deve dichiarare un blocco: è solo un segnale.
        assert!(!script.to_lowercase().contains("block"));
    }
}
