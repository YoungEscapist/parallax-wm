-- parallax compositor config
--
-- Тот же файл по-английски — default_config.lua: те же настройки на тех же
-- строках, комментарии переведены. По умолчанию компоновщик вшивает и кладёт
-- в ~/.config/parallax/config.lua именно АНГЛИЙСКИЙ; чтобы читать справочник
-- по-русски, скопируйте поверх своего config.lua этот.
--
-- This file is Lua. It's evaluated once at startup (and again on
-- Super+Shift+C, "reload_config"). Two functions are available:
--
--   xkb{ layout = "us,ru", variant = "", model = "", options = "" }
--     Keyboard layout(s) for xkbcommon. `layout` is a comma-separated list of
--     layouts to compile into the keymap (the first is active at startup);
--     switch between them at runtime with the layout_next/layout_prev
--     actions below (bound to Ctrl+Space / Ctrl+Shift+Space by default).
--
--   bind{ mods = "super+shift", key = "q", action = "quit", ... }
--     Registers a keybinding. `mods` is a "+"-joined combination of ctrl,
--     alt, shift, super (aliases: control, mod1, logo, mod4, win). `key` is
--     an X11/xkb keysym name (letters/digits, or names like Return, Tab,
--     Left, F1, comma, grave, space). Extra fields depend on the action
--     (see below). A bind requires an EXACT modifier match — e.g. a
--     mods="super" bind will NOT fire while shift is also held.
--
--   set{ cursor_size = 16, cursor_client_max = -1 }
--     Размер курсора. cursor_size — размер стрелки компоновщика и всех форм,
--     которые просят приложения через wp_cursor_shape_v1 (0 = взять из
--     XCURSOR_SIZE). cursor_client_max — потолок для приложений, рисующих
--     курсор своей картинкой (XWayland, GTK3): -1 = потолок равен cursor_size,
--     0 = потолка нет и картинка показывается как прислана, N = потолок N px.
--
--   set{ close_anim = true }
--     Спокойное угасание закрытого окна: снимок последнего кадра гаснет и
--     чуть сжимается к центру за 360 мс. Включено. Ручка нужна потому, что
--     снимок делается offscreen-проходом рендера — тем же приёмом, что и
--     блюр выше, и живьём он ни разу не отсмотрен. Начнёт портить кадр —
--     выключите здесь и нажмите Super+Shift+C.
--
--   set{ blur = false }
--     Размывать фон под островами панели (матовое стекло, как в macOS).
--     ВЫКЛЮЧЕНО ПО УМОЛЧАНИЮ: код написан, но живьём ни разу не отсмотрен, а
--     ошибка в проходе рендера стоит чёрного экрана. Включать, когда под рукой
--     есть tty, чтобы откатиться. Размывается фон (обои), а не вся сцена под
--     плашкой: полноэкранный offscreen-проход на каждый кадр — другая цена.
--
--   set{ infinite_wallpaper = true }
--     Обои живут НА ХОЛСТЕ и едут за камерой, а не приклеены к экрану.
--     Копия ОДНА: она чуть больше экрана и сдвигается затухающе, поэтому
--     повторов и швов не бывает ни при какой камере. false — прежнее
--     поведение (картинка всегда ровно в размер выхода и не двигается вовсе).
--     Если текстуру обоев достать не удалось, parallax молча возвращается к
--     прежнему поведению — без обоев экран был бы чёрным.
--
--   set{ pan_drift = 0.5 }
--     Инерция холста после броска пальцами/мышью: 0 — холст встаёт сразу,
--     1 — едет дольше всего. Шкала логарифмическая по времени доезда, так что
--     0.6 заметно «плавучее» 0.5, а не чуть-чуть. Мягкий скролл тормозит
--     быстрее резкого броска — так и задумано.
--
--   set{ fling_distance = 1.0 }
--     Как далеко улетает БРОШЕННОЕ окно (Win+ЛКМ, отпустить на ходу):
--     1.0 — как задумано (около 2000 px холста на резком броске), 0 — окно
--     встаёт ровно там, где его отпустили, 2.0 — вдвое дальше. Путь растёт
--     вместе со скоростью броска, так что окно можно и подтолкнуть на
--     ладонь, и зашвырнуть в дальний угол. Летящее окно расталкивает те,
--     в которые врезалось (режим коллизии, Super+S).
--
--   set{ anim_speed = 1.0 }
--     Общий темп ВСЕХ анимаций: 1.0 — как задумано, 1.5 — в полтора раза
--     медленнее и спокойнее, 0.7 — резче. Одна ручка на перелёты камеры,
--     зум, сборку тайлинга, разлёт во float и открытие окон сразу; сами
--     длительности живут в anim.rs (`anim::дуг`). Подхватывается
--     перечитыванием конфига (Super+Shift+C), пересборка не нужна.
--
--   set{ bird_eye_key = "space" }
--     Клавиша режима лупы (zoom-nav): Super+<эта клавиша> — тумблер увеличенного
--     вида, где голые стрелки панорамируют холст. Перехватывается ДО биндов,
--     поэтому комбинация Super+<bird_eye_key> для bind{} недоступна.
--
--   dwindle{ preserve_split = true, force_split = 2 }
--     Tiling (Super+T) is Hyprland's dwindle: a new window splits the FOCUSED
--     window's slot in half, so the layout is a BSP tree rather than a fixed
--     chain — closing a window gives its space to its sibling, not to everyone.
--     Same knobs as the dwindle:* section of hyprland.conf, same defaults:
--       split_width_multiplier (1.0) — how much wider than tall a slot must be
--                                      to split left/right instead of top/bottom
--       preserve_split (false)       — false: the split axis is recomputed from
--                                      the slot's proportions on every relayout
--                                      (windows "flip" as neighbours close);
--                                      true: a split stays where it was made
--       force_split (0)              — which half the new window takes:
--                                      0 = the half the cursor is over,
--                                      1 = always left/top, 2 = always right/bottom
--       default_split_ratio (1.0)    — 1.0 = new splits are 50/50
--
-- Available actions and their extra fields:
--   spawn            cmd = "ghostty"
--   quit             (saves session, stops the compositor)
--   restart          (saves session and restarts the compositor in place:
--                     parallax exits with code 42 and launch_native.sh brings it
--                     back up, rebuilding first if the sources are newer than
--                     the binary. Picks up a fresh build without logging out;
--                     clients do NOT survive — the wayland socket dies with
--                     the compositor)
--   kill             (closes the focused window)
--   set_layout       layout = "tile" | "float" | "monocle" | "columns"
--   toggle_layout    (toggles between Float and Tile; ignored in the Super-tap
--                     desktop overview, which manages the camera itself)
--   zoom             (Tile: raise the focused window to the top of the split
--                     tree — Hyprland's "layoutmsg movetoroot"; other layouts:
--                     swap it with the first window in the stack)
--   toggle_floating  (toggle floating for the focused window)
--   toggle_fullscreen (F11: focused window fills the whole screen — no corner
--                     rounding, no shadow, zoom reset to 1:1; toggles back and
--                     restores the previous size, camera and zoom)
--   focus_direction  dx = -1|0|1, dy = -1|0|1 (spatial navigation)
--   focus_stack      dir = 1 | -1 (focus next/prev in stack order)
--   inc_nmaster      n = 1 | -1 (Tile: n>0 flips the nearest split's axis
--                     [togglesplit], n<0 swaps its two halves [swapsplit])
--   set_mfact        delta = 0.05 | -0.05 (Tile: moves the nearest split,
--                     growing the focused window — "layoutmsg splitratio")
--   move_focused     dx = <px>, dy = <px> (floating window move)
--   resize_focused    dw = <px>, dh = <px> (floating window resize)
--   view_tag         tag = 1-9
--   tag_window       tag = 1-9 (assign focused window to tag)
--   toggle_view      tag = 1-9 (toggle tag in current view)
--   toggle_tag       tag = 1-9 (toggle tag on focused window)
--   toggle_minimap
--   toggle_portal
--   toggle_bookmarks_mode  (Super+1-9 jump/save camera bookmarks instead of tags)
--   toggle_snapping    (коллизия: окна расталкивают друг друга при движении)
--   toggle_magnetism   (прилипание края к соседу один раз при отпускании)
--   toggle_fold_stack
--   share_start / share_stop / share_toggle   port = 7373
--                      (мультиюзер: раздать стол гостям, до пяти человек)
--   vt_switch        vt = 1-12
--   layout_next / layout_prev   (cycle keyboard layout)
--   reload_config
--   group_selected     (собрать выделение в созвездие: раскладка «мастер и
--                        стопка» из halley — мастеру 60% ширины во всю высоту,
--                        остальным правая колонка; только Float. Штатно это
--                        делает Win+D по выделению, отдельного бинда нет)
--   ungroup_selected    (распустить созвездие, оставив окна на месте)
--   pin_bookmark_at_cursor  (Alt+B: pin a camera bookmark at the cursor into the
--                        lowest free slot 1-9; jump to it in bookmarks mode with Super+N)
--   column_width_cycle  (Columns/niri mode: cycle active column width 1/3→1/2→2/3→full)
--   toggle_niri_mode    (тумблер Columns ↔ Tile)
--
-- Действия колонок (Columns/niri), все — с теми же именами, что команды niri:
--   window_height_cycle     (switch-preset-window-height)
--   window_height_reset     (reset-window-height)
--   column_width_adjust     percent = ±N   (set-column-width "±N%")
--   window_height_adjust    percent = ±N   (set-window-height "±N%")
--   column_maximize         (maximize-column)
--   column_center           (center-column)
--   column_focus_first / column_focus_last     (focus-column-first/last)
--   column_move_to_first / column_move_to_last (move-column-to-first/last)
--   consume_or_expel_left / consume_or_expel_right
--                           (consume-or-expel-window-left/right)
--   column_toggle_tabbed    (toggle-column-tabbed-display)
--   focus_floating_or_tiling (switch-focus-between-floating-and-tiling)
--   center_focused_column   mode = "never" | "always" | "on-overflow"
--   workspace_step          dir = 1 | -1   (пред/след воркспейс)
--   move_column_to_workspace dir = 1 | -1  (перенести колонку на соседний)
--
-- niri-подобный режим Columns (Super+N): колонки — вертикальные стопки окон,
-- холст скроллится по горизонтали к активной колонке. В этом режиме те же
-- бинды работают по-niri:
--   Super+←/→                focus колонка влево/вправо
--   Super+↑/↓                focus окно внутри колонки (стопки)
--   Super+Ctrl+←/→           переставить колонку
--   Super+Ctrl+↑/↓           переставить окно в колонке
--   Super+comma / period     consume/expel окно в стопку колонки
--   Super+r                  сменить ширину активной колонки
--   Alt+колесо               листать колонки
-- Новое окно открывается отдельной колонкой справа от активной, камера едет
-- к нему.

xkb{ layout = "us,ru", variant = "", options = "grp:alt_shift_toggle" }

-- Interface language / язык интерфейса: "en" (default) or "ru".
--
-- Covers what you read on screen — the bar, the wifi/bluetooth/audio menus,
-- the screenshot and overview hints, notifications — and the replies of the
-- terminal commands (plx-host, the control socket). Re-read on Super+Shift+C,
-- so you can switch without restarting.
--
-- Logs are ALWAYS English and this knob does not touch them: a log ends up in
-- someone else's bug report, and it has to be readable by more than its author.
-- So is what plx-host says on its own behalf (its --help, and its complaint
-- about finding no control socket — printed exactly when there is nobody left
-- to ask which language you chose); what it relays from the compositor follows
-- the knob.
set{ lang = "en" }

-- Клавиша режима лупы (Super+Space по умолчанию).
set{ bird_eye_key = "space" }

-- Курсор одного размера везде: и вне окон, и над ними. Приложения, знающие
-- wp_cursor_shape_v1 (GTK4, Qt6, Chromium), просят у нас ФОРМУ и получают её
-- нашей темой и нашего размера; остальным (XWayland, GTK3) их собственную
-- картинку ужимаем до cursor_client_max. Если нужен крупный курсор из самого
-- приложения (прицел в игре) — cursor_client_max = 0.
set{ cursor_size = 0, cursor_client_max = -1 }

-- Темп анимаций. Правится на вкус и перечитывается по Super+Shift+C:
-- больше единицы — движения дольше и спокойнее, меньше — резче.
set{ anim_speed = 1.0, pan_drift = 0.5, fling_distance = 1.0 }

-- Бесконечные обои: картинка лежит на холсте и едет за камерой (одной копией,
-- с затуханием — швов и повторов не бывает).
set{ infinite_wallpaper = true }

-- Спокойное угасание закрытого окна (см. описание выше).
set{ close_anim = true }

-- Блюр под панелью. Выключен: не отсмотрен живьём, см. описание выше.
set{ blur = false }

-- Свечение окон В ЦВЕТ ОБОЕВ: по краю окна изнутри идёт светящаяся кайма
-- цвета, взятого из палитры текущих обоев (её считает plx-wall и кладёт в
-- ~/.cache/plx-wall/palette.json — тот же цвет, что уходит в терминал и в
-- оболочку). Меняются обои — меняется и свечение, без перезапуска.
--
--   glow       — сила, 0.0…1.0; 0 выключает. Само гаснет на СВЕТЛЫХ обоях:
--                там цветная кайма читается грязью, а не сиянием.
--   glow_width — ширина каймы в логических пикселях (едет за зумом камеры).
--
-- Той же ручкой светится и ОБВОДКА окна — ореол снаружи, наружу он уходит
-- вдвое дальше каймы. Обводка не отдельная вещь: и тень вокруг окна, и кайма
-- внутри считаются ОДНИМ шейдером по одной рамке (src/rounded.rs), поэтому
-- разъехаться им негде, и тень падает ОТ источника света (sun ниже), а не
-- всегда вниз.
--
-- Окно, в котором работают, светится в полную силу, остальные — вполовину
-- тише: так видно, где ввод, даже когда на холсте десяток окон.
--
-- ТОЛЬКО plx-extra (фича `shaders`). В стандартной сборке шейдера нет вовсе,
-- ручка читается как обычно и ничего не делает; в лог при чтении конфигурации
-- уходит строка об этом, чтобы «выставил и не работает» не выглядело поломкой.
set{ glow = 0.0, glow_width = 12.0 }

-- СВЕТ НА ХОЛСТЕ в цвет обоев. Не нарисованное солнце: диска на экране нет.
-- Есть источник, стоящий в точке холста, и от него светятся сцена (мягкая
-- заливка поверх обоев) и ОКНА — сторона, обращённая к свету, ярче, обратная
-- уходит в тень, а кайма (glow выше) горит сильнее там, куда падает. Отсюда и
-- ориентир «откуда светит» на бесконечном холсте, и объём у окон.
--
--   sun       — сила, 0.0…1.0; 0 выключает. Гаснет на светлых обоях.
--   sun_size  — как далеко достаёт свет, в ширинах экрана (радиус спада).
--   sun_x/y   — где источник, в долях экрана от дома монитора. Вне 0…1 можно:
--               холст бесконечен, источник вправе висеть за краем экрана.
--   sun_far   — насколько он ДАЛЕКО: 0 — приклеен к экрану, 1 — лежит на
--               холсте наравне с окнами. Четверть по умолчанию: честно стоящий
--               на холсте источник уходит из кадра после первого же дальнего
--               перелёта, а настоящее солнце позади не остаётся.
--
-- ТОЛЬКО plx-extra (фича `shaders`), как и glow выше.
set{ sun = 0.0, sun_size = 1.6, sun_x = 0.78, sun_y = 0.18, sun_far = 0.25 }

-- КУБ РАБОЧИХ СТОЛОВ, тот самый из Compiz: столы встают на грани призмы,
-- обзор (тап Super) перестаёт быть плоской сеткой. Колесо отодвигает и
-- приближает куб, драг крутит рукой, клик по грани уводит на её стол.
--
-- Куб БЕСКОНЕЧЕН: граней у него cube_faces (четыре, как в Compiz), а столов в
-- кольце сколько угодно — слоты берут столы по кругу и переназначаются на
-- задней грани, которой не видно. Крутить можно вечно, и на десятом столе куб
-- остаётся кубом, а не двадцатигранной стеной.
--
--   cube        — сила, 0.0…1.0; 0 выключает. Ею же множится cube_shade.
--   cube_faces  — сколько граней, 3…12.
--   cube_fill   — какую долю ширины экрана занимает передняя грань.
--   cube_focal  — фокусное расстояние в ширинах экрана: меньше — резче
--                 перспектива.
--   cube_shade  — насколько темнеет дальний край грани; без затемнения куб
--                 читается плоской мозаикой.
--   cube_switch — крутить куб и при переходе на соседний стол (Super+PgUp/
--                 PgDn), а не только в обзоре. Ровно поведение Compiz.
--
-- ТОЛЬКО plx-extra (фича `shaders`). Без неё обзор остаётся плоской сеткой
-- столов, а Super+PgUp/PgDn — обычной сменой стола.
set{ cube = 0.0, cube_faces = 4, cube_fill = 0.62, cube_focal = 2.2 }
set{ cube_shade = 0.35, cube_switch = true }

-- Появление рабочего места при запуске: холст выплывает из темноты и доезжает
-- до своего зума, панель приезжает сверху. Секунда с небольшим, ровно один раз
-- за сеанс. Длительность тянется общим темпом (anim_speed выше).
set{ intro = true }

-- Звук уведомления: короткий тон на каждое всплывающее сообщение — parallax
-- слышит их сам, на сессионной шине, поэтому демон уведомлений может быть
-- любым (mako, dunst, свой).
--   notify_sound  — путь к файлу; пусто = вшитый тон (мягкий стеклянный,
--                   CC0, см. assets/sounds/README.md), "off" = молчать.
--                   Рядом с вшитым лежат ещё два: notify-cloud.ogg потеплее,
--                   notify-polite.ogg — три тихих деревянных щелчка.
--   notify_volume — громкость, 0.0…1.0.
-- Приложение, попросившее тишины подсказкой suppress-sound (плееры на смену
-- трека), звучать не будет.
set{ notify_sound = "", notify_volume = 0.35 }

-- ── VT switching ─────────────────────────────────────────────────────────
for i = 1, 12 do
  bind{ mods = "ctrl+alt", key = "F" .. i, action = "vt_switch", vt = i }
end

-- ── Session / windows ────────────────────────────────────────────────────
-- Mouse: in Float mode, LMB-drag on empty canvas rubber-band-selects windows
-- under the rectangle (plain click with no drag just clears the selection).
-- "kill" closes all selected windows at once if there's a selection, else
-- just the focused window.
bind{ mods = "super+shift", key = "q", action = "quit" }
bind{ mods = "super", key = "q", action = "kill" }
-- Созвездия управляются ТОЛЬКО выделением и Win+D (см. constellation.rs):
-- обвёл окна рамкой — Win+D собрал их в гроздь «мастер и стопка», ещё раз
-- Win+D по ней — распустил. Отдельных super+g / super+shift+g больше нет:
-- две ручки на одно и то же действие только путали, какая из них что делает.
-- Сами действия group_selected / ungroup_selected из конфига никуда не делись
-- — если они нужны на своей клавише, бинд можно вернуть строкой ниже.
bind{ mods = "super", key = "Return", action = "spawn", cmd = "ghostty" }
bind{ mods = "super+shift", key = "Return", action = "zoom" }

-- Полный экран: окно на весь монитор без скруглений и теней, зум 1:1
-- (нужно для видео, игр и демонстрации экрана). Повторное нажатие возвращает
-- окно, камеру и зум обратно.
bind{ mods = "", key = "F11", action = "toggle_fullscreen" }

-- ── Layouts ───────────────────────────────────────────────────────────────
bind{ mods = "super", key = "d", action = "toggle_layout" }
bind{ mods = "super+shift", key = "d", action = "set_layout", layout = "float" }
bind{ mods = "super", key = "t", action = "set_layout", layout = "tile" }
-- Monocle переехал на Super+Shift+M: Super+M занял тумблер магнетизма
-- (см. раздел Toggles), а Shift-вариант тут уже используется для float.
bind{ mods = "super+shift", key = "m", action = "set_layout", layout = "monocle" }
-- niri-подобные колонки (вертикальные стопки, скролл камерой к активной колонке)
bind{ mods = "super", key = "n", action = "toggle_niri_mode" }
-- Тот же Columns, но без тумблера — включить принудительно.
bind{ mods = "super+shift", key = "n", action = "set_layout", layout = "columns" }
-- ── Колонки (Columns) — раскладка и биндинги как в niri ──────────────────
-- Ширина/высота: пресеты niri (⅓ → ½ → ⅔), проценты и сброс.
-- Super+R занят перезапуском компоновщика (см. ниже), поэтому пресеты ширины
-- переехали на Super+Alt+R.
bind{ mods = "super+alt", key = "r", action = "column_width_cycle" }
bind{ mods = "super+shift", key = "r", action = "window_height_cycle" }
bind{ mods = "super+ctrl", key = "r", action = "window_height_reset" }
bind{ mods = "super", key = "minus", action = "column_width_adjust", percent = -10 }
bind{ mods = "super", key = "equal", action = "column_width_adjust", percent = 10 }
bind{ mods = "super+shift", key = "minus", action = "window_height_adjust", percent = -10 }
bind{ mods = "super+shift", key = "equal", action = "window_height_adjust", percent = 10 }
-- Колонка на всю ширину и обратно; колонка по центру экрана.
-- Super+F — ПОИСК ОКНА по имени: буквы видно на экране, Enter перелетает
-- камерой к найденному (и переключает стол, если окно на чужом). См.
-- switcher.rs. Колонка на всю ширину переехала отсюда на Super+Shift+F.
bind{ mods = "super", key = "f", action = "window_search" }
bind{ mods = "super+shift", key = "f", action = "column_maximize" }
-- Super+C занят историей буфера обмена (см. ниже), колонка по центру уехала
-- на Super+Ctrl+C.
bind{ mods = "super+ctrl", key = "c", action = "column_center" }
-- Первая/последняя колонка и перенос колонки в начало/конец полосы.
bind{ mods = "super", key = "Home", action = "column_focus_first" }
bind{ mods = "super", key = "End",  action = "column_focus_last" }
bind{ mods = "super+ctrl", key = "Home", action = "column_move_to_first" }
bind{ mods = "super+ctrl", key = "End",  action = "column_move_to_last" }
-- Забрать окно в колонку / вытолкнуть из неё одной клавишей (niri:
-- consume-or-expel-window-left/right).
bind{ mods = "super", key = "bracketleft",  action = "consume_or_expel_left" }
bind{ mods = "super", key = "bracketright", action = "consume_or_expel_right" }
-- Колонка вкладками: видно только активное окно, слева полоска вкладок
-- (niri: toggle-column-tabbed-display).
bind{ mods = "super+shift", key = "v", action = "column_toggle_tabbed" }

-- Фокус между плавающим слоем и полосой колонок
-- (niri: switch-focus-between-floating-and-tiling). У niri это Mod+Space, но
-- в parallax Super+Space занят режимом лупы (bird_eye_key, перехватывается раньше
-- биндингов), поэтому здесь Super+Shift+Space.
bind{ mods = "super+shift", key = "space", action = "focus_floating_or_tiling" }
-- Как вести вид за активной колонкой: never (по умолчанию, как в niri),
-- always, on-overflow. Меняется на лету.
bind{ mods = "super+alt", key = "c", action = "center_focused_column", mode = "always" }
bind{ mods = "super+alt+shift", key = "c", action = "center_focused_column", mode = "never" }
-- niri-воркспейсы: Super+PageUp/Down переключают воркспейс (в Columns остаёмся
-- в Columns); Super+Ctrl+PageUp/Down переносят активную колонку на соседний.
bind{ mods = "super", key = "Next",  action = "workspace_step", dir = 1 }
bind{ mods = "super", key = "Prior", action = "workspace_step", dir = -1 }
bind{ mods = "super+ctrl", key = "Next",  action = "move_column_to_workspace", dir = 1 }
bind{ mods = "super+ctrl", key = "Prior", action = "move_column_to_workspace", dir = -1 }
bind{ mods = "super", key = "comma", action = "inc_nmaster", n = 1 }
bind{ mods = "super", key = "period", action = "inc_nmaster", n = -1 }
bind{ mods = "super+shift", key = "h", action = "set_mfact", delta = -0.05 }
bind{ mods = "super+shift", key = "l", action = "set_mfact", delta = 0.05 }

-- ── Обзор столов (тап Super) ─────────────────────────────────────────────
-- Столы лежат 2D-сеткой вокруг текущего: новые встают по очереди справа,
-- снизу, слева, сверху от уже занятых ячеек (и только потом по диагоналям).
-- Сами столы не перетаскиваются — расстановку задаёт обзор и сохраняет её
-- между заходами.

-- ── Focus / navigation ───────────────────────────────────────────────────
bind{ mods = "super", key = "Left",  action = "focus_direction", dx = -1, dy = 0 }
bind{ mods = "super", key = "Right", action = "focus_direction", dx = 1, dy = 0 }
bind{ mods = "super", key = "Up",    action = "focus_direction", dx = 0, dy = -1 }
bind{ mods = "super", key = "Down",  action = "focus_direction", dx = 0, dy = 1 }
bind{ mods = "super", key = "j",   action = "focus_stack", dir = 1 }
bind{ mods = "super", key = "Tab", action = "focus_stack", dir = 1 }
bind{ mods = "super", key = "k", action = "focus_stack", dir = -1 }
bind{ mods = "super+shift", key = "Tab", action = "focus_stack", dir = -1 }
-- Alt+Tab — перебор СТОПКИ: окон, лежащих друг под другом в одной точке
-- холста (верхнее закрывает остальные, и Super+стрелки до них не добираются:
-- те ищут соседа в стороне). Порядок фиксируется на первом Tab и держится,
-- пока не отпустят Alt. Под окном никого нет — перебираются все окна стола.
bind{ mods = "alt", key = "Tab", action = "cycle_stack", dir = 1 }
bind{ mods = "alt+shift", key = "Tab", action = "cycle_stack", dir = -1 }

-- ── Move / resize floating windows ───────────────────────────────────────
bind{ mods = "super+ctrl", key = "h",     action = "move_focused", dx = -20, dy = 0 }
bind{ mods = "super+ctrl", key = "Left",  action = "move_focused", dx = -20, dy = 0 }
bind{ mods = "super+ctrl", key = "l",     action = "move_focused", dx = 20, dy = 0 }
bind{ mods = "super+ctrl", key = "Right", action = "move_focused", dx = 20, dy = 0 }
bind{ mods = "super+ctrl", key = "k",  action = "move_focused", dx = 0, dy = -20 }
bind{ mods = "super+ctrl", key = "Up",  action = "move_focused", dx = 0, dy = -20 }
bind{ mods = "super+ctrl", key = "j",   action = "move_focused", dx = 0, dy = 20 }
bind{ mods = "super+ctrl", key = "Down", action = "move_focused", dx = 0, dy = 20 }

-- Note: Super+Shift+H/L are taken by set_mfact above, so resize only binds
-- j/k/arrows here (matches the historical behavior of parallax's hardcoded
-- keymap, where the mfact bind shadowed the resize bind for h/l).
bind{ mods = "super+shift", key = "k",     action = "resize_focused", dw = 0, dh = -30 }
bind{ mods = "super+shift", key = "Up",    action = "resize_focused", dw = 0, dh = -30 }
bind{ mods = "super+shift", key = "j",     action = "resize_focused", dw = 0, dh = 30 }
bind{ mods = "super+shift", key = "Down",  action = "resize_focused", dw = 0, dh = 30 }
bind{ mods = "super+shift", key = "Left",  action = "resize_focused", dw = -30, dh = 0 }
bind{ mods = "super+shift", key = "Right", action = "resize_focused", dw = 30, dh = 0 }

-- ── Tags / workspaces (Super+1-9 view, Super+Shift+1-9 assign) ───────────
-- When bookmarks_mode is on (Super+B), these instead jump to / save camera
-- bookmarks in the same numbered slots.
-- Super+Ctrl+N добавляет/убирает тег из ТЕКУЩЕГО вида (несколько столов на
-- экране разом), Super+Ctrl+Shift+N — из набора тегов сфокусированного окна
-- (окно видно сразу на нескольких столах). Как toggleview/toggletag в dwm.
for i = 1, 9 do
  bind{ mods = "super", key = tostring(i), action = "view_tag", tag = i }
  bind{ mods = "super+shift", key = tostring(i), action = "tag_window", tag = i }
  bind{ mods = "super+ctrl", key = tostring(i), action = "toggle_view", tag = i }
  bind{ mods = "super+ctrl+shift", key = tostring(i), action = "toggle_tag", tag = i }
end

-- ── Toggles ───────────────────────────────────────────────────────────────
bind{ mods = "super", key = "grave", action = "toggle_minimap" }
bind{ mods = "super", key = "p", action = "toggle_portal" }
bind{ mods = "super", key = "b", action = "toggle_bookmarks_mode" }
bind{ mods = "alt", key = "b", action = "pin_bookmark_at_cursor" }
-- Super+S отдан лаунчеру приложений (см. ниже), режим коллизии переехал:
bind{ mods = "super", key = "a", action = "toggle_snapping" }
-- Магнетизм — ОТДЕЛЬНЫЙ тумблер: коллизия расталкивает окна всё время, пока
-- их двигают, а магнетизм срабатывает один раз на отпускании и подравнивает
-- край к соседу. Раньше оба поведения сидели на одном флаге (Super+A), и
-- включить расталкивание без прилипания было нельзя.
bind{ mods = "super", key = "m", action = "toggle_magnetism" }
-- Схлопывание в стопку убрано с Super+Shift+S: комбинация сжимала окна
-- в одну точку, чего от неё не ждали. Нужна — верни строку ниже.
-- bind{ mods = "super+shift", key = "s", action = "toggle_fold_stack" }

-- ── Мультиюзер: раздача стола гостям ─────────────────────────────────────
-- Тумблер: включает раздачу и показывает шестизначный код на панели (справа,
-- «код 123456 · N»), второе нажатие выключает. Гость подключается программой
-- plx-share по адресу этой машины и коду; порт по умолчанию 7373 (задать свой —
-- `port = 1234`). Пока раздача идёт, тайлинг и рабочие столы выключены: у
-- каждого участника своя камера по общему бесконечному холсту.
bind{ mods = "super+shift", key = "s", action = "share_toggle" }

-- ── Keyboard layout switching ────────────────────────────────────────────
bind{ mods = "ctrl", key = "space", action = "layout_next" }
bind{ mods = "ctrl+shift", key = "space", action = "layout_prev" }

-- ── Config reload ─────────────────────────────────────────────────────────
bind{ mods = "super+shift", key = "c", action = "reload_config" }

-- ── Обновление компоновщика (Super+R) ────────────────────────────────────
-- Перезапуск на месте: сессия окон сохраняется в session.json, parallax выходит с
-- кодом 42, а launch_native.sh поднимает его заново — пересобрав, если
-- исходники новее бинаря. Так свежая сборка забирается без перелогина в ly.
-- Окна при этом закрываются: вэйландовый сокет умирает вместе с компоновщиком.
-- Настройки одного config.lua перечитываются дешевле — Super+Shift+C.
bind{ mods = "super", key = "r", action = "restart" }

-- ── Обои (plx-wall) ────────────────────────────────────────────────────────────
-- Win+W открывает меню выбора: карточки обоев + плитка «+» для добавления.
-- Наведи курсор на карточку — в углу появится крестик удаления; то же самое
-- делает правый клик по карточке. Win+Shift+W листает обои без меню.
bind{ mods = "super", key = "w", action = "spawn", cmd = "pkill -USR2 -x plx-wall" }
bind{ mods = "super+shift", key = "w", action = "spawn", cmd = "pkill -USR1 -x plx-wall" }

-- ── Плавающий слой ─────────────────────────────────────────────────────────
-- Win+V: выбранные окна (или сфокусированное) — в плавающий слой и обратно.
-- Работает и в тайлинге, и в ленте niri; окно остаётся в границах своего стола.
bind{ mods = "super", key = "v", action = "float_selected" }

-- ── Закладки камеры ────────────────────────────────────────────────────────
-- Alt+B ставит закладку под курсором в первый свободный слот 1-9,
-- Alt+Win+B убирает ближайшую к курсору. Номера слотов видны на миникарте
-- (Super+` ) рядом с крестиками. Прыжки по закладкам — Super+N в режиме
-- закладок (Super+B).
bind{ mods = "alt+super", key = "b", action = "delete_nearest_bookmark" }

-- Прыжок к закладке камеры: Win+Alt+цифра. Super+цифра занят рабочими столами,
-- а Super+N — лентой niri, поэтому закладкам достался Alt.
for i = 1, 9 do
  bind{ mods = "super+alt", key = tostring(i), action = "jump_bookmark", slot = i }
end

-- ── Блютуз ──────────────────────────────────────────────────────────────────
-- Win+Shift+B (или клавиша XF86Bluetooth, если она есть на клавиатуре) —
-- меню устройств прямо в композиторе, без трея и bluetoothctl.
-- Внутри меню: ↑/↓ (j/k) — выбор, Enter — подключить (незнакомое сначала
-- сопрягает), D — отключить, F — забыть, S — поиск, P — питание адаптера,
-- Esc или клик мимо — закрыть. Клик по строке = Enter по ней.
-- Подтверждение кода при сопряжении показывается там же внизу меню.
bind{ mods = "super+shift", key = "b", action = "bluetooth_menu" }
bind{ mods = "", key = "XF86Bluetooth", action = "bluetooth_menu" }
-- Питание адаптера без открытия меню.
bind{ mods = "super+ctrl", key = "b", action = "bluetooth_power" }

-- При старте сессии поднять адаптер и подключить устройство, которым
-- пользовались последним (адрес запоминается в ~/.local/state/parallax/bluetooth
-- при каждом подключении из меню). set{ bluetooth_autoconnect = false } — выкл.
set{ bluetooth_autoconnect = true }

-- ── Шлем: VR и дополненная реальность ───────────────────────────────────────
-- Окна развешиваются панелями по комнате: тот же композитор, те же столы и
-- бинды, только кадр уходит в шлем (Quest 3 по Wi-Fi через WiVRn, а вообще —
-- любой рантайм OpenXR с XR_MNDX_egl_enable, см. src/vr/).
--
--   Win+Alt+V — надеть шлем и снять его (мониторы при этом работают дальше);
--   Win+Alt+A — passthrough: окна поверх настоящей комнаты;
--   Win+Alt+G — следующая раскладка: дуга → стена → купол → свободно;
--   Win+Alt+H — собрать панели заново вокруг того, куда смотришь.
--
-- Сочетания на Win+Alt, а не на Win+Shift: там уже сидят вкладки колонок (V),
-- меню звука (A) и перечитывание конфига (C), и перевесить их значило бы
-- сломать привычную руку ради режима, который включают раз в день.
--
-- В шлеме: курок контроллера — левая кнопка мыши, хват — тащить панель,
-- стик вперёд/назад — приблизить и отдалить её, вбок — размер. Клавиатура и
-- мышь работают как обычно; без контроллеров указкой служит взгляд.
-- Win+Alt+V — ВЕСЬ вход в шлем: parallax сам поднимет wivrn-server, сам найдёт
-- рантайм OpenXR и будет две минуты ждать, пока ты наденешь Quest и запустишь
-- на нём WiVRn, — и войдёт в VR в ту же секунду, как шлем появится. Нажать
-- ещё раз: пока ждём — отменить ожидание, в шлеме — снять шлем.
-- Есть и сырое действие "vr_toggle" (без сервера и ожидания) — оно для
-- симулятора Monado в харнессе и для отладки.
bind{ mods = "super+alt", key = "v", action = "vr_mode" }
bind{ mods = "super+alt", key = "a", action = "vr_ar" }
bind{ mods = "super+alt", key = "g", action = "vr_layout" }
bind{ mods = "super+alt", key = "h", action = "vr_recenter" }

-- Minecraft: окна parallax панелями в мире игры (см. mine/). Бинд включает режим —
-- дальше нужен запущенный Minecraft с модом plx-mine, который сам подключится к
-- сокету. Выйти — тем же сочетанием: изнутри игры режим не гасится намеренно,
-- иначе панели пропадут, а клавиатура в этот момент у Minecraft.
bind{ mods = "super+alt", key = "m", action = "mine_mode" }

-- layout — раскладка панелей: "arc" (дугой вокруг, по умолчанию), "wall"
--          (плоско перед собой), "dome" (ярусами), "free". Русские имена
--          ("дуга", "стена", "купол", "свободно") разбираются наравне —
--          парсер берёт и те, и другие, см. config.rs;
-- scale  — метров на пиксель окна: 0.0008 даёт окну 1920 полтора метра
--          ширины, то есть примерно монитор на столе;
-- radius — на каком расстоянии стоят панели, метры (0 — считать по
--          охраняемой зоне шлема, её отдаёт сам рантайм);
-- ar     — входить сразу в passthrough;
-- auto   — надевать шлем при старте parallax.
vr{ layout = "arc", scale = 0.0008, radius = 0, ar = false, auto = false }

-- Жесты и кнопки контроллера — обычные действия parallax, те же, что в bind{}.
-- Ключи (полный список — `plxctl vr gestures`):
--   контроллер: menu_button (☰ и нажатие на стик), button_a, button_b,
--               stick_left / stick_right / stick_up / stick_down;
--   рука:       fist (кулак), two_fists, thumb_up (палец вверх), palm_up,
--               pinch_middle / pinch_ring / pinch_little,
--               swipe_left / swipe_right / swipe_up / swipe_down.
-- Щипок большим и УКАЗАТЕЛЬНЫМ здесь не значится: он левая кнопка мыши,
-- то есть выбор окна и нажатие кнопок, и переназначать его нечем.
-- Значение — имя действия строкой или таблица с action и аргументами.
-- Что не перечислено, работает по умолчанию: кулак и ☰ — пульт «Пуск»,
-- A/X — клавиатура, B/Y — терминал, вбок — соседний стол, вверх — обзор
-- столов, вниз — свести камеру на окно, палец вверх — окно на весь экран,
-- мизинец — passthrough, два кулака — пересобрать сцену вокруг взгляда.
vr{ gestures = {
      fist         = "vr_launcher",
      thumb_up     = "toggle_fullscreen",
      swipe_left   = { action = "workspace_step", dir = -1 },
      swipe_right  = { action = "workspace_step", dir = 1 },
      swipe_up     = "toggle_overview",
      swipe_down   = "center_window",
      pinch_little = "vr_ar",
      two_fists    = "vr_recenter",
} }

-- ── Полка состояния ─────────────────────────────────────────────────────────
-- Win+Shift+P (или клик по вертикальной полосочке справа от панели столов) —
-- ряд со значками: блютуз, вайфай, звук со шкалой, батарея (если она есть),
-- сон, перезагрузка, выключение. Клик по значку блютуза открывает его меню,
-- по вайфаю — включает и выключает радиомодуль, по значку звука — глушит,
-- по шкале — ставит громкость на месте нажатия. Кнопки питания срабатывают
-- со ВТОРОГО клика: первый взводит (значок краснеет), второй выполняет.
-- Esc или клик мимо — закрыть.
bind{ mods = "super+shift", key = "p", action = "tray_menu" }

-- ── Wi-Fi and sound ─────────────────────────────────────────────────────────
-- Win+Shift+I — network picker: Enter connect (asks for the password on a new
-- protected network), D disconnect, F forget, S rescan, P radio, Esc close.
-- Win+Shift+A — sound devices: Enter makes the device default AND moves the
-- streams already playing, M mute, -/+ volume, Esc close.
-- Both also open from the tray: left click on the icon opens the menu, right
-- click does the quick thing (wi-fi radio, mute).
bind{ mods = "super+shift", key = "i", action = "wifi_menu" }
bind{ mods = "super+shift", key = "a", action = "audio_menu" }


-- ── Лаунчер приложений ──────────────────────────────────────────────────────
-- fuzzel в цветах Void (тема: ~/.config/fuzzel/fuzzel.ini).
-- Тумблер: второе нажатие гасит уже открытый лаунчер, а не плодит второй.
bind{ mods = "super", key = "s", action = "spawn", cmd = "pkill -x fuzzel || fuzzel" }

-- ── Снимок экрана и буфер обмена ────────────────────────────────────────────
-- PrtScr: снимок ВСЕГО экрана прямо в буфер обмена, файл никуда не пишется.
-- grim отдаёт png в stdout, wl-copy забирает его оттуда.
bind{ mods = "", key = "Print", action = "spawn", cmd = "grim - | wl-copy" }
-- Super+C: история буфера списком в fuzzel — и картинки, и текст. Выбранное
-- снова кладётся в буфер (cliphist decode | wl-copy), то есть вставляется
-- обычным Ctrl+V туда, куда нужно.
--
-- Саму историю набивают два сторожа wl-paste, они поднимаются в
-- launch_native.sh вместе с сессией: без них список всегда пуст.
-- Тумблер, как у лаунчера: второе нажатие гасит открытый список.
bind{ mods = "super", key = "c", action = "spawn",
      cmd = "pkill -x fuzzel || cliphist list | fuzzel --dmenu | cliphist decode | wl-copy" }

-- ── Мониторы ────────────────────────────────────────────────────────────────
-- monitor{ name = "DP-2", width = 2560, height = 1080, refresh = 200 }
--   name    — имя коннектора из /sys/class/drm (DP-1, DP-2, HDMI-A-1) либо
--             модель из EDID ("Redmi 30 HFCW"). Коннектор надёжнее.
--   width/height/refresh — режим. Точное совпадение по размеру, ближайшая к
--             refresh частота из тех, что коннектор реально отдаёт. Если
--             такого размера у коннектора НЕТ, parallax строит его сам по CVT и
--             отдаёт ядру: меньший режим железо растягивает своим скейлером на
--             всю матрицу. Так на 4K-панели получается настоящий FullHD —
--             вчетверо меньше пикселей на отрисовку, а не просто крупнее
--             интерфейс. Если и синтезированный режим не примут, вернёмся на
--             PREFERRED (в логе будет warn). Больше физической матрицы просить
--             бесполезно — такой запрос отклоняется сразу.
--             refresh без width/height = «тот же размер, что у PREFERRED, но с
--             этой частотой». Ничего не задано = как было.
--   x/y     — где монитор стоит РЯДОМ С СОСЕДЯМИ (по умолчанию — левее самого
--             левого, слева направо в порядке подключения). Это не место на
--             холсте (у каждого монитора свой прямоугольник холста, см.
--             monitors::ШАГ_ДОМА) — раскладка нужна ровно для перехода курсора
--             через край экрана (Super переносит мышь на монитор снизу/сверху/
--             сбоку) и не двигает окна и рабочие столы.
--   scale   — 1.0 по умолчанию. Делит ЛОГИЧЕСКИЙ размер стола, но НЕ режим:
--             интерфейс становится крупнее, а сканаут и композитинг остаются
--             прежними. Это про читаемость (DPI), не про нагрузку — если нужна
--             именно нагрузка, задавайте width/height.
--   transform — normal | 90 | 180 | 270 | flipped | flipped-90/180/270.
--   primary — true делает монитор активным при старте, даже если ядро отдало
--             его коннектор не первым (порядок сканирования DRM непостоянен —
--             без этого флага «основной» монитор менялся от сеанса к сеансу).
--             На всю раскладку должен быть только один primary.
-- Применяется при подключении коннектора, то есть на старте и на горячем
-- втыкании кабеля; reload_config (Super+Shift+C) режим уже не переставляет.
--
-- Redmi 30 HFCW (DP-2): 2560x1080, EDID отдаёт 60 Гц как PREFERRED и 200 Гц
-- вторым detailed timing — без этой строки композитор вставал на 60. Он же
-- основной монитор, стоит в раскладке сверху.
monitor{ name = "DP-2", width = 2560, height = 1080, refresh = 200, x = 0, y = 0, primary = true }
-- BOE105HDR (HDMI-A-1): стоит снизу, по центру относительно ультраширокого —
-- (2560 - 1920) / 2 = 320.
monitor{ name = "HDMI-A-1", x = 320, y = 1080 }

-- ═══════════════════════════════════════════════════════════════════════════
-- ЖЕСТЫ ТАЧПАДА (gesture{}) — модель driftwm, перенесённая в parallax 30.08.2026
-- ═══════════════════════════════════════════════════════════════════════════
--
-- Жест — такой же бинд, как клавиатурный, только триггер у него пальцы:
--
--   gesture{ mods = "alt", fingers = 3, kind = "swipe",
--            where = "window", action = "resize-window" }
--
--   mods    — как у bind{}: "alt", "super", "shift", "ctrl" и их сочетания
--   fingers — сколько пальцев (2–5; 2 приходят прокруткой, parallax это скрывает)
--   kind    — swipe, swipe-up/down/left/right, doubletap-swipe,
--             pinch, pinch-in, pinch-out, hold
--   where   — window (под курсором окно), canvas (пусто), anywhere (по умолчанию)
--   action  — непрерывное или пороговое, см. ниже
--
-- НЕПРЕРЫВНЫЕ действия едут за пальцами каждый кадр и вешаются только на
-- swipe/pinch (не на направленные и не на pinch-in/out — те срабатывают раз):
--   pan-viewport  — вести вид (только swipe)
--   zoom          — зум камеры (только pinch)
--   move-window, move-snapped-windows      — вести окно
--   resize-window, resize-window-snapped   — менять размер окна
-- ПОРОГОВЫМ может быть любое действие из списка bind{}, плюс center-nearest.
--
-- Пороги распознавания (имена как в driftwm, переносятся копированием):
--   set{ swipe_threshold = 12.0, pinch_in_threshold = 0.85, pinch_out_threshold = 1.15 }
--
-- ───────────────────────────────────────────────────────────────────────────
-- ВСЁ НИЖЕ ЗАКОММЕНТИРОВАНО НАМЕРЕННО, и это не лень.
--
-- Пока таблица пуста, жесты обрабатывают прежние ветки input.rs — то есть
-- поведение parallax ровно такое, каким было до появления gesture{}. Стоит
-- раскомментировать строку, и её жест переходит к таблице ЦЕЛИКОМ, отменяя
-- встроенный. Там, где это отменяет что-то существующее, стоит пометка
-- «ПЕРЕБИВАЕТ» — читать перед тем, как включать.
-- ───────────────────────────────────────────────────────────────────────────

-- ВНИМАНИЕ (01.09.2026): всё, что ниже, теперь РАБОТАЕТ БЕЗ ВАС — этот список
-- встроен в parallax как умолчание (`ЖЕСТЫ_ПО_УМОЛЧАНИЮ` в config.rs) и включается
-- после вашего конфига. Строки оставлены здесь как справочник и как заготовка
-- для правки: свой `gesture{}` с тем же триггером, модификаторами и контекстом
-- ОТМЕНЯЕТ умолчание, а не добавляется к нему. Чтобы жеста не было вовсе,
-- повесьте на него `action = "none"`.

-- ── Над окном ──────────────────────────────────────────────────────────────
-- gesture{ mods = "alt", fingers = 3, kind = "swipe", where = "window", action = "resize-window" }
-- gesture{ mods = "alt+shift", fingers = 3, kind = "swipe", where = "window", action = "resize-window-snapped" }
-- gesture{ mods = "alt", fingers = 3, kind = "pinch-in", where = "window", action = "toggle_fullscreen" }
-- gesture{ mods = "alt", fingers = 3, kind = "pinch-out", where = "window", action = "toggle_fullscreen" }

-- ── По холсту ──────────────────────────────────────────────────────────────
-- Голый двухпальцевый щипок сейчас не делает НИЧЕГО (встроенный зум просит
-- Alt), так что эта строка ничего не отменяет — она чистое добавление.
-- gesture{ fingers = 2, kind = "pinch", where = "canvas", action = "zoom" }

-- ── Везде ──────────────────────────────────────────────────────────────────
-- ПЕРЕБИВАЕТ: в Columns голый свайп тремя пальцами листает полосу и столы.
-- gesture{ fingers = 3, kind = "swipe", action = "pan-viewport" }
-- ПЕРЕБИВАЕТ: в Columns четыре пальца сейчас листают полосу наравне с тремя.
-- gesture{ fingers = 4, kind = "swipe", action = "center-nearest" }
-- gesture{ mods = "super", fingers = 3, kind = "swipe", action = "center-nearest" }
-- gesture{ mods = "super", fingers = 2, kind = "pinch", action = "zoom" }
-- gesture{ fingers = 3, kind = "pinch", action = "zoom" }
-- gesture{ fingers = 4, kind = "pinch-out", action = "toggle_overview" }
-- gesture{ mods = "super", fingers = 3, kind = "pinch-out", action = "toggle_overview" }
-- gesture{ fingers = 4, kind = "hold", action = "center_window" }
-- gesture{ mods = "super", fingers = 3, kind = "hold", action = "center_window" }

-- ── Чего из driftwm перенести НЕ ВО ЧТО ────────────────────────────────────
-- Эти его действия в parallax не существуют, и придумывать им смысл было бы хуже,
-- чем сказать прямо:
--   fit-window, fit-window-snapped   — «вырасти в свободное место»
--   zoom-to-fit, zoom-to-fit-snapped — «вписать всё в экран»
-- Триггеры под них есть (pinch-in/out на 2 и 4 пальца) — нужны сами действия.
--
-- Отдельно: 3-finger-doubletap-swipe (тап тремя, потом свайп) требует задержки
-- среднего клика с тачпада, а средний клик в parallax уже занят — им, например,
-- останавливают раздачу с панели. Триггер разбирается, но пока не срабатывает.

-- ═══════════════════════════════════════════════════════════════════════════
-- АВТОДОВОД КУРСОРА ПО КРАЯМ ТАЧПАДА
-- ═══════════════════════════════════════════════════════════════════════════
--
-- Накладка кончается раньше, чем экран: ведя окно через весь холст, палец
-- упирается в край, и движение просто прекращается. Автодовод замечает палец
-- В КРАЕВОЙ ЗОНЕ накладки и продолжает вести курсор сам — тем быстрее, чем
-- глубже палец зашёл.
--
-- Работает везде и само собой: движение идёт обычным путём указателя, поэтому
-- его одинаково видят перетаскивание окна, выделение рамкой и любой захват.
-- Речь именно про край НАКЛАДКИ, а не экрана.
--
-- Требует чтения тачпада напрямую (libinput сырых координат пальцев не отдаёт),
-- дескриптор берётся через тот же сеанс, что и у libinput. Нет тачпада — нет и
-- автодовода, никаких сообщений об этом не будет.
--
--   touchpad_edge_motion — включён ли (по умолчанию да)
--   touchpad_edge_zone   — доля накладки от края, считающаяся краем
--                          (0.08 = 8 %, примерно ширина пальца; потолок 0.4)
--   touchpad_edge_speed  — пикселей в секунду на самом краю
--
--   touchpad_edge_only_drag — доводить ТОЛЬКО когда что-то тащат (по умолчанию
--                          нет: просили «чтобы работал везде»). Включи, если
--                          палец, лежащий на кромке, будет уводить курсор сам.
--
-- set{ touchpad_edge_motion = true, touchpad_edge_zone = 0.08,
--      touchpad_edge_speed = 900.0, touchpad_edge_only_drag = false }

-- ─────────────────────────────────────────────────────────────────────────────
-- МЫШИНЫЕ АККОРДЫ (mouse{}) — модель hevel, перенесена 05.09.2026
--
-- hevel (git.sr.ht/~dlm/hevel) — плавающий скроллящийся WM, в котором мышью
-- делается вообще всё: команда задаётся не кнопкой, а ПАРОЙ кнопок, нажатых
-- подряд. Кнопки нумеруются как у него: 1 — левая, 2 — колесо, 3 — правая.
--
--   mouse{ chord = "1-3", action = "spawn_rect", cmd = "ghostty" }
--
-- Первая цифра — кнопка, которую держат, вторая — которую нажимают следом.
--
-- Действия аккордов:
--   spawn_rect     — обвести рамку и открыть в ней окно (нужно поле cmd)
--   close_under    — закрыть окно, НАД КОТОРЫМ ОТПУСТИЛИ (жертва выбирается
--                    по дороге, а не в начале)
--   pan            — вести камеру протяжкой
--   move_window    — вести окно
--   resize_window  — менять окну размер
-- Плюс ЛЮБОЕ действие из общей таблицы (те же, что вешаются на клавиши) —
-- оно срабатывает сразу на второй кнопке.
--
-- ЧЕМ ЭТО ПЛАТИТСЯ. Аккорд опознаётся по ВТОРОЙ кнопке, поэтому первую нельзя
-- сразу отдавать приложению — иначе «3-1» успело бы открыть контекстное меню.
-- Первое нажатие ЗАДЕРЖИВАЕТСЯ до mouse_chord_timeout (по умолчанию 250 мс,
-- имя и значение из hevel). Не дождались второй кнопки или отпустили ту же —
-- приложение получает обычный клик, просто чуть позже.
--
-- Задержка ИЗБИРАТЕЛЬНА: задерживается только кнопка, с которой хоть один
-- аккорд НАЧИНАЕТСЯ. Поэтому строки ниже закомментированы, и поэтому среди них
-- нет ни одного аккорда на «1-…»: включив такой, вы задержите каждый обычный
-- левый клик. Хотите полный набор hevel — раскомментируйте всё; хотите не
-- трогать левую кнопку — оставьте только «2-…» и «3-…».
--
-- Пустая таблица (ничего не раскомментировано) = мышь работает ровно так, как
-- работала до появления этого раздела. Это то же обещание, что даёт gesture{}.
--
-- mouse{ chord = "1-3", action = "spawn_rect", cmd = "ghostty" }
-- mouse{ chord = "3-1", action = "close_under" }
-- mouse{ chord = "3-2", action = "pan" }
-- mouse{ chord = "2-1", action = "move_window" }
-- mouse{ chord = "2-3", action = "resize_window" }
-- mouse{ chord = "1-2", action = "toggle_fullscreen" }
-- set{ mouse_chord_timeout = 250 }
