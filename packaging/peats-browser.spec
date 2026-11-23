%global debug_package %{nil}
%global _build_id_links none

Name:           peats-browser
Version:        0.1.0
Release:        1%{?dist}
Summary:        Browser GTK4 con blocco contenuti, spazi isolati e vista affiancata

License:        MIT
URL:            https://github.com/peats/browser
BuildArch:      x86_64

# Il binario viene compilato con `cargo build --release` e copiato nella
# cartella SOURCES prima della creazione del pacchetto: lo spec non ricompila.
Requires:       webkitgtk6.0%{?_isa} >= 2.52
Requires:       gtk4%{?_isa}
Requires:       libadwaita%{?_isa}

%description
Peats è un browser leggero per GTK4/libadwaita costruito su WebKitGTK.

Include blocco contenuti nativo con liste aggiornabili ed eccezioni per sito,
protezione antitracciamento (ITP e cookie di terze parti), assistenza ai banner
dei consensi, spazi di lavoro con dati di navigazione isolati, vista affiancata
con due schede, suggerimenti nella barra indirizzi (schede, cronologia,
segnalibri e ricerca rapida) e ripristino della sessione.

%prep

%build

%install
install -Dpm0755 %{_sourcedir}/peats-browser %{buildroot}%{_bindir}/peats-browser
install -Dpm0644 %{_sourcedir}/io.github.peats.Browser.desktop \
    %{buildroot}%{_datadir}/applications/io.github.peats.Browser.desktop
install -Dpm0644 %{_sourcedir}/io.github.peats.Browser.svg \
    %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/io.github.peats.Browser.svg

%files
%{_bindir}/peats-browser
%{_datadir}/applications/io.github.peats.Browser.desktop
%{_datadir}/icons/hicolor/scalable/apps/io.github.peats.Browser.svg

%changelog
* Sun Nov 22 2026 Peats <noreply@example.com> - 0.1.0-1
- Prima confezione: navigazione, privacy, spazi isolati, vista affiancata e
  suggerimenti nella barra indirizzi.
