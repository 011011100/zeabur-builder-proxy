# 概念驗證（PoC）

在一個 Kubernetes 叢集上跑通 Dockerfile 路徑（polling 觸發）與 Railpack builder：controller 透過
Git smart HTTP 解析 `Build` 的 ref，每個新 commit 建立一個 `BuildRun`，每個 run 建立一個 Job；Job
取得 commit 後以 `buildctl` 連到**該 namespace 自己的 buildkitd**，推到叢集內的 registry，digest
最後寫進 `BuildRun.status` 與 `Build.status.latestImage`。

## 狀態

| 項目 | 狀態 |
| --- | --- |
| Dockerfile 建置、polling 觸發 | 在測試叢集上驗證 |
| Railpack 建置（M1） | 在測試叢集上驗證 |
| 失敗：ref 不存在、Dockerfile 不存在 | 驗證：`SourceReady=False RefNotFound`；`BuildRun` Failed `BuildFailed` |
| 每個 namespace 一個 buildkitd、閒置縮到 0 | 在測試叢集上驗證（[下方](#namespace-隔離)） |
| namespace 之間的隔離 | 在測試叢集上驗證，`scripts/poc-isolation.sh` |
| 單元測試 | 57 個通過 |

## 需求

- Kubernetes 1.33 以上，才能用 `hostUsers: false`（user namespace）；較舊的叢集設 `HOST_USERS=true`。
- 會執行 NetworkPolicy 的 CNI（k3s 內建）。
- **磁碟**：node 上至少還有 8 GB 可用空間。buildkitd 的 cache、它拉的 base image 與 registry 都用
  node 的磁碟；可用空間低於 kubelet 的門檻時，kubelet 會驅逐 node 上所有的 Pod。每個 namespace
  的 buildkitd 有自己的 cache。
- 叢集能連到 Git host、Docker Hub 與 ghcr.io。
- `nix develop`（kubectl、k3d）與 Docker。

## 重現

```sh
nix develop
scripts/build-images.sh zeabur-builder dev     # 依本機架構建三個 image
# k3d：匯入 image，部署，然後等 whoami 這個 Build
K3D_CLUSTER=<cluster> scripts/poc.sh <context> whoami
# Railpack 範例
scripts/poc.sh <context> hello-python
# namespace 隔離：先讓 poc-a 與 poc-b 各建置一次，再執行檢查
NAMESPACES="builder-poc poc-a poc-b" scripts/poc.sh <context>
scripts/poc-isolation.sh <context>
```

`scripts/poc.sh` 套用 `deploy/crds.json` 與 `deploy/poc/` 的 manifest，替 `NAMESPACES` 裡每個 namespace
產生一個 registry 帳號，然後等每個指定 Build 的 `status.latestImage`，遇到第一個失敗的 `BuildRun`
就結束。重跑時沿用既有的密碼。環境變數列在腳本開頭。

查看結果：

```sh
kubectl -n builder-poc get build -o wide        # Ready、Revision、Image
kubectl -n builder-poc get buildrun             # Phase、Reason、Started、Finished
kubectl -n builder-poc logs job/<buildrun>      # buildctl 的進度
kubectl -n builder-system get sts,pvc           # 每個 namespace 的 buildkitd 與 cache
```

namespace 有 `builder-system`（controller 與所有 buildkitd）、`builder-registry` 與 `builder-poc`
（範例 Build）。全部移除：

```sh
kubectl delete namespace builder-poc poc-a poc-b builder-system builder-registry
kubectl delete clusterrole,clusterrolebinding zeabur-builder
kubectl delete clusterbuilder dockerfile railpack
kubectl delete -f deploy/crds.json
```

## 範例 Build

| Build | 來源 | 預期 |
| --- | --- | --- |
| `whoami` | `traefik/whoami@master`，用它的 Dockerfile | Succeeded |
| `hello-python` | `dockersamples/helloworld-demo-python@main`，沒有 Dockerfile | 經 Railpack Succeeded |
| `no-such-ref` | 不存在的 branch | `SourceReady=False`、`RefNotFound`，沒有 BuildRun |
| `no-such-dockerfile` | 不存在的 Dockerfile 路徑 | BuildRun Failed |

## 決定

- **每個 namespace 一個 buildkitd**（plan §7）：第一次有 BuildRun 要開始時，controller 在
  `builder-system` 建立 `buildkitd-<namespace>` 的 StatefulSet、headless Service、ingress
  NetworkPolicy 與憑證 Secret。Pod 還沒 Ready 時 BuildRun 維持 Pending（`BuildkitdStarting`）。
  閒置超過 `IDLE` 秒（預設 600）縮到 0，cache PVC 保留；namespace 刪除後一併清掉。
- **預設 `hostUsers: false`**：rootful `moby/buildkit:v0.34.1` 加 `privileged: true`，在 user
  namespace 中跑完上面所有建置；buildkitd 的 `/proc/self/uid_map` 為 `0 3195928576 65536`，容器內的
  root 對應到 host 上沒有權限的 UID 範圍。`HOST_USERS=true` 退回單純的 privileged 容器。
- **`RUN` 的網路**：`networkMode = "bridge"`，每個 `RUN` 有自己的網路 namespace（`10.10.0.0/16`），
  經 masquerade 用 buildkitd Pod 的 IP 外出，所以 `deploy/poc/buildkitd.yaml` 的 egress
  NetworkPolicy 也管得到 `RUN`：只允許 DNS、registry 與私有網段以外的位址。
- **buildkitd 設定**：`insecure-entitlements = []`；gateway frontend 只允許
  `docker/dockerfile`、`docker/dockerfile-upstream` 與 Railpack 的 frontend。
- **Registry**：Zot v2.1.22，plain HTTP，沒有持久化（`emptyDir`）。每個 namespace 一個 htpasswd 使用者，
  只能寫 `<namespace>/**`，任何人都能讀。帳號放在 `builder-system` 的 `registry-push-<namespace>`；
  controller 以 `--registry-auth-prefix=registry-push-` 找到它，複製成每個 run 自己的 Secret。
  buildkitd 在設定中把 registry 標為 insecure，所以 jobspec 不需要特例。
- **mTLS**：每個 namespace 一個 CA，server 與 client 憑證放在同一個 Secret
  `buildkitd-<namespace>-tls`，client 憑證複製到 build namespace 的 `zeabur-builder-buildkit-client`。
- **Provenance**：jobspec 沒有傳 `attest:provenance`，所以用 buildctl 的預設值。plan 預定的設定是
  `mode=min`，尚未設定。
- **buildkitd cache**：每個 namespace 一個 PVC；GC 讓 cache 不超過磁碟區的 80%，並在磁碟可用空間
  低於 20% 時清理，早於 kubelet 的驅逐門檻。

## 測試叢集

`zeabur-test-4` VM 上的單節點 k3d 叢集：k3s v1.36.5、containerd 2.3.4、kernel 7.0、arm64、4 CPU、
5 GB 記憶體。image 在 VM 上建置，以 `k3d image import` 匯入。

下面「結果」與「耗時」是改成每個 namespace 一個 buildkitd 之前，在 `zeabur-test` VM（2 CPU、4 GB）
上測的；當時四個範例 Build 同時開始，共用一個 buildkitd 與兩顆 CPU。

### 結果

| Build | Revision | Image |
| --- | --- | --- |
| `whoami` | `1ce75d0` | `…/builder-poc/whoami:master-1792608027-1ce75d0@sha256:687e297d70a419afb58641107ba75f3235dcb7abe33422ea8a93d259d5ffd73f` |
| `hello-python` | `62c0894` | `…/builder-poc/hello-python:main-1792608027-62c0894@sha256:3894b8d8dd5d57341368a0fa25fd224beea3cc831ca89ab21eecd8683e777337` |

`…` 是 `registry.builder-registry.svc.cluster.local:5000`。registry 列得出兩個 tag 與 `buildcache`
tag；controller 替每個 run 發出 `BuildSucceeded` event。

### 耗時

冷是空的 buildkitd cache 與 registry；熱是刪掉再重建 Build 後，用冷的那次留下的 cache 建同一個
commit。端到端是從 `BuildRun` 建立到 `completionTime`。

| Build | Cache | Fetch | Plan | 建置與推送 | 端到端 | 命中 cache 的步驟 |
| --- | --- | --- | --- | --- | --- | --- |
| `whoami` | 冷 | 1.0 s | – | 34.3 s | 67 s¹ | 18 個中 0 個 |
| `whoami` | 熱 | 1.0 s | – | 18.7 s | 23 s | 18 個中 8 個 |
| `hello-python` | 冷 | 1.0 s | 7.0 s | 55.9 s | 67 s | 24 個中 0 個 |
| `hello-python` | 熱 | 1.0 s | 5.0 s | 3.4 s | 14 s | 24 個中 14 個 |
| `no-such-dockerfile` | – | 1.0 s | – | 0.1 s | 5 s | – |

¹ 含一次 30 秒的重試：同時開始的 run 搶著建立 buildkitd 的 client Secret。已修正；熱的那次沒有重試。

熱的 `whoami` image digest 與冷的相同。熱的 `hello-python` image digest 不同（`sha256:fae44b3f…`），
原因還沒查。

### 磁碟

最高時用了 node 約 4.5 GB：buildkitd 的 cache 3.0 GB、registry 0.76 GB、k3d 中的 image 約 0.4 GB，
以及它們在 VM Docker 中的副本 0.58 GB。`buildctl prune --all` 釋出 3.2 GB。node 的可用空間要遠高於
kubelet 的驅逐門檻：之前在只剩約 5 GB 的磁碟上試，node 進入 DiskPressure，上面所有 Pod 都被驅逐。

## namespace 隔離

在 `zeabur-test-4` 上以 `NAMESPACES="builder-poc poc-a poc-b" IDLE=120` 部署。

**同時建置**：在 `poc-a` 與 `poc-b` 同時建立 `whoami`。兩個 BuildRun 立刻進入 Pending
`BuildkitdStarting`（`waiting for buildkitd-poc-a-0 to be ready`），各自的 buildkitd 7 秒後 Ready，
兩個建置在 `buildkitd-poc-a-0` 與 `buildkitd-poc-b-0` 上同時進行（Building 01:01:18 到 01:01:47），
都成功，`status.buildkitdInstance` 分別是自己的 Pod。

**冷啟動**：從 BuildRun 建立到 controller 把它交給 Job（`buildkitdInstance` 寫入）：

| 情況 | Pod 建立到 Ready | BuildRun 等待 |
| --- | --- | --- |
| 第一次，要新建 PVC（local-path） | 7 秒 | 8.2 秒 |
| 縮到 0 之後，PVC 已存在 | 3 秒 | 5.1 秒 |

**閒置縮到 0**：最後一個 run 結束約 2 分鐘後，controller 記錄 `idle; buildkitd scaled to zero`，
StatefulSet 變成 `0/0`，三個 PVC 都留著。下一次建置的 cache 仍在。

**閒置記憶體**（每個 buildkitd，建置結束後 15 到 100 秒間取樣，數值穩定）：

| 量測 | 值 |
| --- | --- |
| cgroup `memory.stat` anon | 約 21 到 22 MiB |
| `kubectl top`（working set） | 約 45 MiB |
| cgroup `memory.current`（含 page cache） | 約 290 MiB，其中約 270 MiB 是可回收的檔案快取 |

**namespace 刪除**：建立 `poc-c`、讓它的 buildkitd 啟動後刪掉 namespace；下一次檢查（7 秒內）
刪除了它的 StatefulSet、Service、NetworkPolicy、憑證 Secret 與 cache PVC。管理員建立的
`registry-push-poc-c` 不會被刪。

**沒有 registry 帳號的 namespace**：BuildRun 維持 Pending `RegistryCredentialsMissing`（`no Secret
registry-push-poc-c with push credentials in builder-system`），也沒有建立 buildkitd。

**`scripts/poc-isolation.sh`**：以 `poc-a` 的成員身分（拿得到 `poc-a` 的 client 憑證與推送帳號），
用 `buildctl` 直接連 buildkitd。全部通過：

| 檢查 | 結果 |
| --- | --- |
| `poc-b` 的憑證連 `poc-b` 的 buildkitd | 成功 |
| `poc-a` 的憑證連 `poc-b` 的 buildkitd | 拒絕：`tls: failed to verify certificate: x509: certificate signed by unknown authority` |
| `poc-a` 的 Pod 連 `poc-b` 的 buildkitd | 連不上：`connection refused`（ingress NetworkPolicy） |
| `poc-a` 的帳號推到 `…/poc-a/probe` | 成功 |
| `poc-a` 的帳號推到 `…/poc-b/probe` | 拒絕：`denied: requested access to the resource is denied` |
| `RUN` 的網路位址 | `10.10.0.3/16`，不是 Pod 的位址 |
| `RUN` 連 `https://example.com`、registry | 成功 |
| `RUN` 連其他 namespace 的 Service、`kubernetes.default.svc`、`169.254.169.254` | 都沒有 HTTP 回應 |
| `RUN --network=host` | 拒絕：`network.host is not allowed` |
| `# syntax=docker.io/tonistiigi/dockerfile` | 拒絕：`is not an allowed gateway source` |
| 對照：一般 Pod 連同一個 Service 與 `169.254.169.254` | 成功 |

`169.254.169.254` 是腳本在 node 的 loopback 上加的假 metadata endpoint（port 8169；k3d 的 Traefik
佔了 port 80），結束時移除。

## 已知缺口

- Ready 在第一個 run 結束前都是 `Unknown`（`NotBuilt`）；ref 不存在只顯示在 `SourceReady`。
- Git：`credentialsRef`（token 或 deploy key，plan §5.3）只有單元測試與本機 SSH server 的測試涵蓋，還沒在叢集上跑過。
- 觸發：每個 Build 各自 polling 與 `rebuild` annotation；沒有 webhook、沒有 repo 層級的 poller。
  annotation 是在上面這些結果之後加的，只有單元測試涵蓋。
- Controller：單一 replica，沒有 leader election。幾個 run 同時 reconcile 時，namespace 的並行數可能
  超過上限一個。縮到 0 與新 run 開始之間也可能互相競爭；新 run 的建立時間算作活動，所以窗口很小。
- buildkitd：`buildkitd.toml` 改了之後，要等各 namespace 的 buildkitd 下次啟動才生效。縮到 0 時
  Pod 以 `Error` 狀態結束（buildkitd 收到 SIGTERM 後以非 0 結束），不影響 cache。
- 網路：egress 只處理 IPv4；`RUN` 仍能經 kube-dns 解析叢集內的名稱。叢集用公有網段時要自己加進
  NetworkPolicy。
- 憑證：沒有輪替，CA 金鑰不保存。
- Registry 帳號：由管理員（PoC 中是 `scripts/poc.sh`）建立，controller 不產生；新增 namespace 時
  Zot 的 ACL 要重新載入。Zot 在 kubelet 同步 Secret 後（約一分鐘）才看得到新密碼。
- RBAC：Secret 仍是叢集範圍，用來複製 client 憑證與推送帳號。
- Registry：plain HTTP；Pod 重啟後 image 就沒了。
- 通知：只支援 `keel`，在上面這些結果之後加的；`flux-receiver` 與 `generic` 只記錄 log 後略過；
  沒有 `notify_errors_total` 指標。
- sandbox runtime（`--buildkitd-runtime-class`）沒有在 gVisor 或 Kata 上試過。
- 不在範圍內：zbplan（M4）、GitHub App。
