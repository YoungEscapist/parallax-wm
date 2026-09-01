# assets/

## Nunito

`Nunito-Regular.ttf`, `Nunito-SemiBold.ttf` — статические начертания Nunito,
взятые с `fonts.gstatic.com` через CSS API v1 (старый User-Agent заставляет
Google отдать один TTF на начертание целиком, а не порезанный по unicode-range):

```
curl -A "Mozilla/4.0" "https://fonts.googleapis.com/css?family=Nunito:400,600&subset=latin,cyrillic"
```

Лицензия — SIL Open Font License 1.1, полный текст в `Nunito-OFL.txt`. Шрифт
вшивается в бинарь через `include_bytes!` (см. `src/text.rs`), то есть
распространяется вместе с dawn — файл лицензии обязан лежать рядом.

Покрытие проверено по таблице cmap обоих файлов: ASCII 0x20..0x7E целиком,
кириллица 0x400..0x45F целиком, «…» (0x2026), «№» (0x2116), «°» (0x00B0).
Символа питания «⏻» (0x23FB) в шрифте нет — но значки dawn рисует не шрифтом,
а битмап-масками (`text::bitmap_fit`), так что это ничего не задевает.
