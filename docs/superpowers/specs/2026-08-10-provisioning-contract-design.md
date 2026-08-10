# TASK-57: контракт provisioning в домене

Дизайн доменного контракта провизионинга: что именно VMLord обещает донести до
Linux-гостя и как это выражено типами в `crates/core`. Первая подзадача эпика #3
«Provisioning Linux»; всё, что идёт следом — генерация cloud-config (#58),
ISO9660-writer (#59), сборка seed в конвейере (#61), — читает эти типы.

## Задача

`VmCreateRequest` уже несёт `username`, `password`, `ssh_enabled` и
`ssh_deploy_key` (`crates/core/src/lib.rs:16-28`), но нативный путь их
игнорирует: `VmCreationPipeline::create` (`crates/platform/src/create.rs:65`)
создаёт пустой VHDX и цепляет ISO вторым SCSI-attachment'ом, а систему ставит
человек. Правила имени пользователя при этом живут в UI
(`crates/ui/src/lib.rs:847-863`), то есть в слое, которому по AGENTS.md бизнес-
логика запрещена, и домен о них не знает.

В объём входят: тип контракта, источник образа, профиль дистрибутива как данные,
валидация рядом с `VmCreateRequest::validate()` и правка всех слоёв под новую
форму запроса. Вне объёма — генерация cloud-config, работа с seed, всё
Windows-специфичное и перестройка формы создания VM под облачный путь (#65).

## Ключевое решение: локальный носитель и облачный образ — разные вещи

Локальный ISO означает ручную установку системы человеком. Облачный образ
означает cloud-init, который получает конфигурацию из seed. Provisioning имеет
смысл ровно во втором случае, поэтому он не отдельное поле рядом с источником, а
часть облачного варианта: «локальный образ с заданным паролем» тогда нельзя
сконструировать, и правило не нужно проверять в рантайме.

```rust
pub enum VmSource {
    /// Установочный носитель. Систему ставит человек.
    LocalMedia { path: String },
    CloudImage { image: CloudImage, provisioning: Provisioning },
}

pub struct CloudImage {
    pub profile: DistroProfile,
    pub release: String,
}

pub struct Provisioning {
    pub username: String,
    pub password: Option<Password>,
    pub ssh: SshAccess,
    pub locale: String,
    pub keyboard: String,
    pub timezone: String,
}

pub enum SshAccess {
    Disabled,
    Enabled { deploy_key: bool },
}

/// Plaintext-пароль, который не печатается.
pub struct Password(String);
```

`VmCreateRequest` теряет пять полей (`image_path`, `username`, `password`,
`ssh_enabled`, `ssh_deploy_key`) и получает одно: `source: VmSource`.

## Модули

`core::distro` — данные о дистрибутивах, переезжают целиком из
`crates/image/src/distro.rs`: `DistroProfile`, `UBUNTU`, методы `image_url`,
`checksums_url` и `file_name`. Это склеивание строк без единого обращения к сети,
поэтому `core` от переезда не приобретает ни одной зависимости. В `image`
остаётся `validated_release`: формат релиза — свойство URL, который там строится,
а не домена. `image` ре-экспортирует `DistroProfile` и `ubuntu`, чтобы его
вызывающие не приобретали зависимость от `core` ради профиля.

Одно изменение при переезде: поля профиля становятся `String` вместо
`&'static str`, а `const UBUNTU` — функцией `ubuntu() -> DistroProfile`. Причина —
задача #67, загрузка профилей из JSON: разобранный файл не даёт `&'static str`
иначе как утечкой памяти, так что правка неизбежна, и дешевле сделать её сейчас,
пока профиль читают три места. Следствие: `ResolvedImage.default_user` и
`admin_group` в `crates/image/src/resolve.rs` тоже становятся `String`, а
`crates/image/tests/resolve.rs` переходит с `UBUNTU` на `ubuntu()`.

`core::provisioning` — сам контракт и его валидация.

## Пароль

До хеширования (#61) в контракте лежит plaintext, а `VmCreateRequest` выводится
через `{:?}`. `Password` поэтому реализует `Debug` вручную и печатает
`Password(<redacted>)`, `Display` не реализует вовсе. Утечка в лог становится
невозможной по построению, а не отлавливаемой тестом постфактум; существующий
`omits_request_secrets` (`crates/platform/src/hcs_config.rs:494`) остаётся вторым
рубежом.

Пароль необязателен: `None` означает вход только по ключу, и в seed уедет
`ssh_pwauth: false` (#58). Комбинация «пароля нет и SSH выключен» отвергается —
такая VM создастся, загрузится и не пустит внутрь никого.

## Валидация

`VmCreateRequest::validate()` сохраняет нынешние проверки (имя, RAM, диск, ядра)
и добавляет `source.validate()`.

| Что | Правило |
| --- | --- |
| `LocalMedia.path` | непустой (сегодняшняя проверка `image_path`, дословно) |
| `CloudImage.release` | непустой; формат — дело `validated_release` в `image` |
| `Provisioning.username` | непустое, ≤32 символов, первый символ — строчная латиница или `_`, дальше строчные, цифры, `_`, `-` |
| `Provisioning.password` | если `Some`, то непустой |
| пароль и SSH | не могут быть выключены оба |
| `locale`, `keyboard`, `timezone` | непустые, без управляющих ASCII-символов |

Правило имени пользователя переносится из `crates/ui/src/lib.rs:847-863` дословно
и в UI не остаётся. Управляющие символы запрещены потому, что эти три строки
уезжают в YAML cloud-config и в `write_files` для `/etc/default/keyboard` (#58):
перевод строки в значении там уже не опечатка, а инъекция в документ.

Профиль дистрибутива не валидируется: сегодня он константа в коде. Проверки его
полей появятся вместе с загрузкой из JSON (#67), где источник данных перестанет
быть доверенным.

В UI остаётся то, что домену не принадлежит: совпадение пароля с подтверждением
(свойство формы, не VM) и уникальность имени среди существующих VM (нужен
список).

Логирование: отказ — `log::warn!` в точке отказа, потому что это ошибка
пользователя, а не сбой системы; принятый провизионинг — `log::debug!` со сводкой
без секретов.

## Следствия по слоям

**Платформа.** `HcsVmConfigBuilder::build` отвечает на `CloudImage` отказом с
указанием задачи #61 — тем же приёмом, каким сегодня отвергаются GPU-режимы и
`External`/`Internal` (ARCHITECTURE.md, «VM update contract»). `LocalMedia`
обрабатывается ровно как сейчас: проверка существования файла
(`crates/platform/src/create.rs:128`), `HcsGrantVmAccess`, второй SCSI-attachment.

**Legacy-бэкенд.** `crates/legacy-backend/src/windows.rs:251-269` передаёт в
AppSandbox путь к ISO вместе с именем пользователя, паролем и SSH-флагами: его
модель — это «локальный носитель с provisioning», ровно то состояние, которое
новый тип делает невыразимым. Осознанная потеря: под `VMLORD_BACKEND=legacy`
`LocalMedia` уезжает в AppSandbox с пустыми учётными данными и `ssh_*` = 0, то
есть без unattended-ответов iso-patch. Legacy-бэкенд объявлен временным
(AGENTS.md, «Project Status»), а #66 всё равно отвязывает сборку от iso-patch;
держать `Option<Provisioning>` в `LocalMedia` ради него значит вписать в домен
форму кода, который мы удаляем.

**UI.** С формы создания уходят виджеты имени пользователя, пароля, подтверждения
и двух SSH-чекбоксов: после решения выше их значение не доходит ни до одного
бэкенда, а форма, собирающая данные и молча их выбрасывающая, хуже отсутствующей.
`create_vm_request` строит `VmSource::LocalMedia`. Задача #65 перестраивает эту
форму под облачный путь и возвращает поля туда, где они работают.

## Тесты

В `core::provisioning` — по паре позитив/негатив на каждое правило таблицы, плюс
два теста на секреты: `Debug` для `Password` даёт `<redacted>`, и `{:?}` от целого
`VmCreateRequest` не содержит plaintext. В `hcs_config.rs` добавляется тест на
отказ по `CloudImage`. Существующие тесты `hcs_config.rs`, `create.rs`, `app` и
`crates/platform/tests/hyperv.rs` правятся механически под новую форму запроса.

Сборка проверяется `cargo build --target=x86_64-pc-windows-gnu`; тесты `core` и
`image` гоняются на хосте.
