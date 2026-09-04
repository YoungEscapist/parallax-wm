# assets/

## Обложка

`cover.jpg` — 1024×1024, заголовок обоих README. Созвездие: точки на двух
планах, ближний ярче и смещён относительно дальнего — это и есть параллакс,
от которого имя. Сгенерировано Gemini, выбрано Яриком 03.09.2026.

`social-preview.png` — то же самое в 1280×640 под «Social preview» GitHub
(Settings → General → Social preview). Размер не выдуман: у GitHub картинка
превью обрезается до 2:1, и квадрат он срезал бы сверху и снизу прямо по
рисунку. Поэтому здесь созвездие вписано по высоте, а поля добраны фоном
оригинала (#0f1014) — обрезать диагональную композицию нечем.

Пересобрать превью из обложки, если она поменяется:

```python
from PIL import Image
src = Image.open("cover.jpg").convert("RGB")
W, H, поле = 1280, 640, 40
сторона = H - поле * 2
холст = Image.new("RGB", (W, H), (15, 16, 20))
холст.paste(src.resize((сторона, сторона), Image.LANCZOS),
            ((W - сторона) // 2, (H - сторона) // 2))
холст.save("social-preview.png", optimize=True)
```

## Nunito

`Nunito-Regular.ttf`, `Nunito-SemiBold.ttf` — статические начертания Nunito,
взятые с `fonts.gstatic.com` через CSS API v1 (старый User-Agent заставляет
Google отдать один TTF на начертание целиком, а не порезанный по unicode-range):

```
curl -A "Mozilla/4.0" "https://fonts.googleapis.com/css?family=Nunito:400,600&subset=latin,cyrillic"
```

Лицензия — SIL Open Font License 1.1, полный текст в `Nunito-OFL.txt`. Шрифт
вшивается в бинарь через `include_bytes!` (см. `src/text.rs`), то есть
распространяется вместе с parallax — файл лицензии обязан лежать рядом.

Покрытие проверено по таблице cmap обоих файлов: ASCII 0x20..0x7E целиком,
кириллица 0x400..0x45F целиком, «…» (0x2026), «№» (0x2116), «°» (0x00B0).
Символа питания «⏻» (0x23FB) в шрифте нет — но значки parallax рисует не шрифтом,
а битмап-масками (`text::bitmap_fit`), так что это ничего не задевает.
