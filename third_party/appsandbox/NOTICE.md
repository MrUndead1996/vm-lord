# AppSandbox legacy backend

VMLord temporarily ships the Windows x64 AppSandbox runtime. The backend is
loaded dynamically and is an implementation detail behind the
`vmlord-legacy-backend` crate.

| Runtime file | Source | SHA-256 |
| --- | --- | --- |
| `appsandbox_core.dll` | `C:\sources\appsandbox\bin\Release\appsandbox_core.dll` | `CEE2F16A2ABA00583F05CBA2321C033F55A18AC933B47502419FE2DBEDDF0E6F` |
| `iso-patch.exe` | `C:\sources\appsandbox\bin\Release\iso-patch.exe` | `32E2C63464E16C7585B9EE89E7B5D815EB687A9B840825D8156C347040D7C051` |

`iso-patch.exe` is required to create Linux VHDX images and is staged beside
`vmlord.exe` with the backend DLL. macOS components, the AppSandbox desktop
application, WebView UI, and display resources are not part of this runtime
package.
