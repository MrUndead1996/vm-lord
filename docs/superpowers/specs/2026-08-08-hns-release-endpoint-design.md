# TASK-46: освобождение HNS endpoint при остановке VM

Дизайн жизненного цикла endpoint'а между остановкой и повторным запуском VM.
Часть декомпозиции эпика #5 «Миграция HNS: базовая сетевая интеграция»; опирается
на #41 (endpoint виртуальной машины) и #38 (подключение endpoint'а при старте),
обе уже в `main`.

## Задача

На реальном Hyper-V после Force Stop следующий старт пересоздаёт compute system
с сохранённым endpoint'ом и падает с HRESULT 0x803B0014 «Эта конечная точка уже
подключена к коммутатору». Endpoint остаётся привязан к compute system, которую
HCS уже уничтожил, и второй раз подключить его некуда.

Сегодня `VmForceStopPipeline` вызывает только `HcsTerminateComputeSystem`, а
`VmStartPipeline` не различает причины отказа старта.

## Что делает AppSandbox

Комментарий в задаче указывает на `hcs_detach_network` как на reference-функцию.
Проверка исходников: функция объявлена в `hcs_vm.h:140`, определена в
`hcs_vm.c:1472` и **не вызывается ниоткуда**. Её комментарий («hot-detach the
network adapter from a running VM so that HCS can deliver SystemExited») —
непроверенная гипотеза, а не описание работающего кода.

Реальная модель AppSandbox другая: endpoint создаётся на каждый старт и
удаляется на каждую остановку (`asb_vm_cleanup_network`, `asb_core.c:946`;
вызовы из обработчика `SystemExited` `asb_core.c:876`, из Force Stop
`asb_core.c:3432` и из delete `asb_core.c:3465`). 0x803B0014 там не возникает
потому, что endpoint никогда не переиспользуется. Стабильность адреса
обеспечивается не переиспользованием, а явным статическим IP в
`IpConfigurations` (`hcn_network.c:462`): NAT получает `<base>.2`, при конфликте
последний октет инкрементится.

`hcs_stop_vm` (graceful) осознанно ничего не отцепляет: «Don't detach network or
modify the VM — let it shut down undisturbed».

## Решения

### Модель остаётся своей, не AppSandbox'овой

Эпик #5 решил, что endpoint живёт до удаления VM, а адрес назначает HNS IPAM.
Переход на модель AppSandbox (endpoint на время запуска + собственный аллокатор
адресов) отвергнут: он отменяет решение эпика и требует согласования с #47
(DHCP) и #42 (очистка), тогда как наблюдаемая поломка лечится точечно.

### Detach только перед terminate

`HcsShutDownComputeSystem` асинхронен: он возвращается, когда запрос доставлен
гостю, а не когда гость выключился. Момента «гость ещё жив, но сеть уже не
нужна» у VMLord нет, а detach до запроса оставил бы отказавшийся выключаться
гость работать без сети и без способа её вернуть. Graceful shutdown поэтому не
трогается — так же, как в AppSandbox.

Вариант «detach по событию `SystemExited`» отвергнут: к моменту события compute
system уже разрушается, гарантий нет, а fallback на старте всё равно нужен.

### Recovery на старте

Detach недоступен там, где compute system уже уничтожена: крах гостя,
самостоятельный shutdown, рестарт VMLord. Единственный оставшийся рычаг —
сам endpoint, поэтому старт пересоздаёт его.

Чтобы адрес гостя при этом не менялся, новый endpoint создаётся с явным
`IpConfigurations`, содержащим адрес и префикс старого. Это единственное место,
где VMLord называет адрес явно, и он не выбирает его, а удерживает выданный HNS:
решение эпика «VMLord не второй аллокатор адресов» остаётся в силе.

Если адрес прочитать не удалось, endpoint создаётся без него и `WARN` говорит,
что адрес гостя сменился. VM, которая не может стартовать, хуже смены адреса —
тот же компромисс, что уже принят в `ensure_endpoint` для endpoint'а,
потерянного при сбросе HNS.

### Неудачный detach не отменяет остановку

Force Stop терминирует VM даже при неудавшемся detach'е и возвращает `Ok`;
диагностика — `WARN` с HRESULT и последствием («следующий старт пересоздаст
endpoint»). Force Stop обязан оставаться последним средством остановить зависшую
VM, а последствие неснятого адаптера уже покрыто recovery на старте.

Это решение владельца проекта, расходящееся с формулировкой пункта задачи «не
маскировать ошибку detach успешным Force Stop»: буквально ошибка не поднимается
в результат, а остаётся в логе.

## Компоненты

### `HcsSystem::remove_network_adapter` (`hcs.rs`)

```rust
pub fn remove_network_adapter(&self, endpoint_id: Uuid) -> Result<(), RepositoryError>
```

Отправляет `HcsModifyComputeSystem` и ждёт завершения операции с собственным
`DETACH_TIMEOUT` (30 с — бюджет против зависшей службы, не против гостя):

```json
{"ResourcePath":"VirtualMachine/Devices/NetworkAdapters/3F2B0C11-...","RequestType":"Remove"}
```

Ключ адаптера в пути обязан совпадать с ключом, который
`hcs_config::apply_network_adapter` пишет в `config.json` (`format!("{:?}", GUID)`
— верхний регистр, без скобок). Расхождение молча ничего не отцепляет, поэтому
форматирование выносится в `hcs_config::adapter_key(endpoint_id)` и вызывается
обеими сторонами.

### Detach в `VmForceStopPipeline` (`force_stop.rs`)

Новый шов рядом с существующим `system_terminator`:

```rust
Fn(&str /* compute system id */, Uuid /* endpoint */) -> Result<(), RepositoryError>
```

Порядок: найти mapping → при `NetworkMode::Nat` и `endpoint_id == Some(id)`
отцепить адаптер → терминировать. Остальные режимы и VM без записанного
endpoint'а идут прямо к terminate.

Продакшн-реализация открывает compute system заново, отдельно от той, что
открывает `terminate_hcs_system`: два открытия дешевле, чем шов, в котором
порядок detach/terminate перестаёт быть проверяемым. `HCS_E_SYSTEM_NOT_FOUND`
означает «отцеплять уже нечего» и ошибкой не считается.

### Recovery в `VmStartPipeline` (`start.rs`)

Шов старта начинает различать причину отказа:

```rust
pub(crate) enum StartFailure {
    /// 0x803B0014 от create или start: endpoint занят чужой compute system.
    EndpointBusy(RepositoryError),
    Failed(RepositoryError),
}
```

Шов endpoint'а получает политику:

```rust
pub(crate) enum EndpointPolicy { Reuse, Replace }
Fn(&str, Option<Uuid>, EndpointPolicy) -> Result<VmNetworkAdapter>
```

`start()` при `EndpointBusy` и `NetworkMode::Nat` один раз повторяет цепочку
`attach_network(Replace)` → grant → start. Второй `EndpointBusy` — отказ. VM без
NAT и обычный `Failed` не повторяются.

Продакшн-реализация `Replace`: `HcnEndpoint::open_if_present(id)` → прочитать
адрес и префикс из `HcnQueryEndpointProperties` → `HcnEndpoint::delete(id)` →
создать новый endpoint с новым `Uuid` и прочитанным адресом. Запись нового
`endpoint_id` в `MetadataStore` и переписывание `config.json` идут по уже
существующему пути `attach_network` — отдельного кода для этого не появляется.

## Документация

Абзац `ARCHITECTURE.md` про неотцепляемый адаптер («Whether HCS still delivers
`SystemExited` … VMLord attaches the adapter and does not detach it») заменяется
описанием жизненного цикла endpoint'а: detach перед terminate, graceful shutdown
без detach'а и почему, recovery на старте с удержанием адреса. Там же
фиксируется, что `hcs_detach_network` в AppSandbox мёртв и его обоснование про
`SystemExited` не подтверждено.

## Тестирование

Unit-тесты:

* документ detach'а называет endpoint и `RequestType: Remove`;
* `adapter_key` даёт тот же ключ, что и секция `NetworkAdapters`;
* force stop отцепляет адаптер до terminate;
* force stop пропускает detach для `NetworkMode::None` и для VM без
  `endpoint_id`;
* неудавшийся detach не отменяет terminate и не превращается в отказ;
* старт повторяется ровно один раз при `EndpointBusy`;
* пересозданный endpoint попадает и в mapping, и в `config.json`;
* второй `EndpointBusy` — отказ старта;
* `Failed` не повторяется, VM без NAT не повторяется;
* настройки endpoint'а с запрошенным адресом содержат `IpConfigurations`;
* адрес и префикс читаются из свойств, сообщённых HNS, а их отсутствие даёт
  `None`.

`#[ignore]`-тест в `crates/platform/tests/hyperv.rs`: start → Force Stop →
start. Проверяет, что второй старт проходит, `endpoint_id` не изменился и адрес
остался прежним. Ручная parity-проверка на Hyper-V — за владельцем проекта.

## Вне объёма

* **Очистка endpoint'ов** при удалении VM и при `initialize` — #42.
* **Встроенный DHCP** — #47.
* **`VmSummary.ip_address`** — #37.
* **Graceful shutdown через собственного гостевого агента** — не в этом эпике;
  сегодняшний `HcsShutDownComputeSystem` остаётся как есть.
