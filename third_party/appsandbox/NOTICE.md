# AppSandbox legacy backend

VMLord temporarily ships the Windows x64 AppSandbox runtime. The backend is
loaded dynamically and is an implementation detail behind the
`vmlord-legacy-backend` crate.

| Runtime file | Source | SHA-256 |
| --- | --- | --- |
| `appsandbox_core.dll` | `C:\sources\appsandbox\bin\Release\appsandbox_core.dll` | `CEE2F16A2ABA00583F05CBA2321C033F55A18AC933B47502419FE2DBEDDF0E6F` |

`appsandbox_core.dll` is staged beside `vmlord.exe` by the build. It is the
only file of this package VMLord ships: `iso-patch.exe`, AppSandbox's host-side
Ubuntu installer, was dropped once the native backend began building disks from
cloud images. macOS components, the AppSandbox desktop application, WebView UI,
and display resources are not part of this runtime package either.
