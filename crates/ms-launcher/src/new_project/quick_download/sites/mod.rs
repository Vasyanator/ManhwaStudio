/*
File: src/launcher/new_project/quick_download/sites/mod.rs

Purpose:
Module map of the per-site chapter resolvers. One file per supported site; this file only
declares them.

Key submodules:
- naver, webtoons, mangadex, readcomiconline, comicfury, kuaikan, bato
- manganelo (four mirrors), mangapark (mirror family), weebdex, mangataro, dankefuerslesen
- dynastyscans, kaliscan, hentai2read, tcbscans, rawkuma, mangafreak, dandadan
- hiperdex, komikcast, mangaread, senmanga, weebcentral

Notes:
Site knowledge (hosts, endpoints, markers, decoders, ordering) must stay inside these files.
Dispatch to them happens in `plan.rs`; see MODULE_README.md.
*/

pub(super) mod bato;
pub(super) mod comicfury;
pub(super) mod dandadan;
pub(super) mod dankefuerslesen;
pub(super) mod dynastyscans;
pub(super) mod hentai2read;
pub(super) mod hiperdex;
pub(super) mod kaliscan;
pub(super) mod komikcast;
pub(super) mod kuaikan;
pub(super) mod mangadex;
pub(super) mod mangafreak;
pub(super) mod manganelo;
pub(super) mod mangapark;
pub(super) mod mangaread;
pub(super) mod mangataro;
pub(super) mod naver;
pub(super) mod rawkuma;
pub(super) mod readcomiconline;
pub(super) mod senmanga;
pub(super) mod tcbscans;
pub(super) mod webtoons;
pub(super) mod weebcentral;
pub(super) mod weebdex;
