//! Assistenza per i banner di consenso dei CMP riconosciuti.
//!
//! Il browser clicca **solo** il pulsante di rifiuto esplicitamente previsto da
//! un CMP presente in questa tabella, e solo quando la funzione è attiva.
//! Non cerca pulsanti per testo o aspetto, non interpreta il banner e non
//! preme mai «Accetta»: se il CMP non è in tabella, o il pulsante non esiste,
//! la pagina resta com'è e decide l'utente.
//!
//! Copertura verificata solo con pagine di prova locali che riproducono gli
//! identificatori indicati: non è una verifica sui siti reali, dove i CMP
//! possono cambiare markup o mostrare il banner in un iframe cross-origin.

/// CMP supportati: nome leggibile e selettore CSS del pulsante di rifiuto.
pub const CMPS: &[(&str, &str)] = &[
    ("OneTrust", "#onetrust-reject-all-handler"),
    (
        "Cookiebot",
        "#CybotCookiebotDialogBodyButtonDecline",
    ),
    ("Didomi", ".didomi-continue-without-agreeing"),
];

pub fn cmp_names() -> Vec<&'static str> {
    CMPS.iter().map(|(name, _)| *name).collect()
}

/// Script iniettato all'avvio del documento. Esegue al massimo un clic per
/// selettore e per caricamento, con un numero limitato di tentativi.
pub fn script() -> String {
    let selectors = CMPS
        .iter()
        .map(|(_, selector)| format!("\"{selector}\""))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#"(function () {{
  if (window.__peatsBannerHelper) {{ return; }}
  window.__peatsBannerHelper = true;
  var selectors = [{selectors}];
  var clicked = {{}};
  var attempts = 0;
  function visible(element) {{
    if (!element) {{ return false; }}
    var style = window.getComputedStyle(element);
    if (style.visibility === "hidden" || style.display === "none") {{ return false; }}
    var box = element.getBoundingClientRect();
    return box.width > 0 && box.height > 0;
  }}
  function tryReject() {{
    attempts += 1;
    for (var i = 0; i < selectors.length; i++) {{
      var selector = selectors[i];
      if (clicked[selector]) {{ continue; }}
      var element = document.querySelector(selector);
      if (!element || !visible(element) || element.disabled) {{ continue; }}
      try {{
        element.click();
        clicked[selector] = true;
      }} catch (error) {{}}
    }}
    if (attempts >= 8) {{
      clearInterval(timer);
      observer.disconnect();
    }}
  }}
  var observer = new MutationObserver(tryReject);
  try {{
    observer.observe(document.documentElement, {{ childList: true, subtree: true }});
  }} catch (error) {{}}
  var timer = setInterval(tryReject, 1500);
  tryReject();
}})();"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_uses_known_selectors_only() {
        let script = script();
        for (_, selector) in CMPS {
            assert!(script.contains(&format!("\"{selector}\"")), "{selector}");
        }
        assert!(script.contains("MutationObserver"));
        assert!(script.contains("querySelector(selector)"));
    }

    #[test]
    fn script_never_accepts() {
        let script = script().to_lowercase();
        assert!(!script.contains("accept"));
        // Nessuna ricerca per testo: si usano solo i selettori della tabella.
        assert!(!script.contains("textcontent"));
        assert!(!script.contains("innertext"));
        assert_eq!(script.matches("queryselector").count(), 1);
        for (_, selector) in CMPS {
            let selector = selector.to_lowercase();
            assert!(!selector.contains("accept"));
        }
    }

    #[test]
    fn supported_cmps_are_documented() {
        let names = cmp_names();
        assert_eq!(names, vec!["OneTrust", "Cookiebot", "Didomi"]);
    }
}
