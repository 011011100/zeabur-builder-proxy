# PLAN：Kubernetes 原生輕量建置 Operator（Zeabur Builder）

> 本文件是設計決策紀錄＋實作計畫。它改寫自原始計畫，依可行性研究（[feasibility.md](feasibility.md)）與以下決定調整：
>
> - **專案範本**：沿用 Zeabur Identity 的 Rust 範本（Cargo workspace、Nix flake、lint、CI、DCO）。
> - **名稱**：API group `builder.zeabur.com`、專案名 `zeabur-builder`、crate 前綴 `zeabur-builder-*`。`registry.internal` 仍是範例。
> - **語言**：controller 用 Rust（kube 4.2）；Job 使用原生 `buildctl` 加上一個 Rust 小包裝程式，見 §17 ADR-9。
> - 與原始計畫不同之處以 **〔變更〕** 標示；原本標記 `[待驗證]` 的項目，結果寫在 §16。
> - 不確定的設計問題，請回頭問使用者，不要自行擴大範圍。

---

## 1. 目標與範圍

### 1.1 要做的事

把 Git commit 轉成 container image digest，然後交給下游工具 rollout。

- 宣告式定義「建什麼」：Git 來源、builder、輸出 image。
- 支援三種 builder：**Dockerfile**、**Railpack**、**zbplan**（AI 生成 Dockerfile）。
- 推送到內部或外部 OCI registry。
- 觸發來源：**GitHub App**（webhook，可回報 Checks）與**一般 Git repo**（polling）。
- 輸出穩定、可排序的 tag 加上 digest，讓 Keel、Flux Image Automation、Flux Operator ResourceSet、Argo CD Image Updater、Kargo 等工具可以接手。
- 輸出 Prometheus 指標。

### 1.2 明確不做的事（Non-goals）

| 不做 | 理由／替代方案 |
|---|---|
| 通用 pipeline、多步驟 DAG、跑測試 | 範圍鎖死在 source → image；需要額外步驟請寫在 Dockerfile 或 railpack.json |
| 部署或 rollout | 交給 Keel、Flux 等工具 |
| PR 建置與 preview image | v1 不做；未來以新的 provider 事件加上動態 BuildRun 擴充 |
| Log 收集與保存 | 交給使用者既有的 log collector；我們只負責讓 log 收得到、找得到 |
| 依賴 Tekton 或 Shipwright | 太重，見 §17 ADR-1 |
| 預設使用 sandbox runtime（gVisor、Kata） | **〔變更〕** 隔離單位是 namespace（§1.3）；要替不受信任的客戶建置，管理員須另外指定 runtime class（§7.2） |

### 1.3 使用者與信任模型

~~使用者是公司內部團隊，視為可信任。安全措施的目的是防止意外，而不是防範惡意攻擊。~~

**〔變更〕** 不同 namespace 可能屬於不同團隊，彼此不該讀到對方的程式碼、secret 與 cache，也不該推到對方的 image 路徑。隔離的單位是 **namespace**：

- 每個 build namespace 有自己的 buildkitd 與 cache（§7.1）、自己的 CA 與 client 憑證（§7.3），以及只能推到 `<defaultRegistry>/<namespace>/**` 的 registry 帳號（§8）。
- 同一個 namespace 內的建置彼此信任：它們共用 buildkitd 與 cache，一個建置的 `RUN` 讀得到同一個 namespace 其他建置的 cache mount。
- `RUN` 有自己的網路 namespace，連得到外網與 registry，連不到 Pod、Service、node 與 cloud metadata endpoint（§7.2）。
- buildkitd 仍是 rootful 加 `privileged`，靠 user namespace（`hostUsers: false`）讓容器內的 root 對應到 host 上沒有權限的 UID。這擋得住意外，也擋得住一般的跨團隊存取，但**擋不住刻意利用核心漏洞逃出容器的攻擊**。

所以不同團隊可以共用一個叢集；但若任何人都能推程式碼進來（不受信任的客戶），這樣還不夠：buildkitd 要跑在 sandbox runtime（gVisor、Kata 等，以 `--buildkitd-runtime-class` 指定），最好再搭配專用的 node pool。這個選項預設關閉：sandbox runtime 會讓建置變慢，也不是每個叢集都有。

---

## 2. 架構總覽

```
           GitHub (webhook)                Git server (plain)
                 │                                 ▲ ls-remote (polling)
                 ▼                                 │
┌─────────────────────────── controller (1 Deployment, leader election) ───────────────────────────┐
│  webhook receiver ──wake──▶ Build reconciler ──create──▶ BuildRun reconciler ──create──▶ Job     │
│  poller (repo-dedupe)  ──▶   (resolve ref→SHA)          (quota, wake buildkitd)                 │
│  source providers: githubApp | plainGit        metrics /metrics        GitHub Checks reporter   │
└───────────────────────────────────────────────────────────────────────────────────────────────────┘
                                                              │
         Job Pod: init fetch → init plan → main build ────────┼── gRPC + mTLS ──▶ buildkitd-<namespace>
                                                              │                  (one per namespace, PVC cache)
                                                              ▼                          │
                                                    termination message (JSON)          push
                                                                                         ▼
                                                                               OCI registry (image + plan artifact)
                                                                                         │
                                                              Keel / Flux / Argo CD Image Updater / Kargo
```

**元件清單**

| 元件 | 形式 | 職責 |
|---|---|---|
| controller | 1 個 Deployment（leader election） | 兩個 reconciler、webhook 接收、polling、公平排程、Checks 回報、指標 |
| buildkitd | **〔變更〕** 每個 build namespace 1 個 StatefulSet（1 個 replica、1 個 PVC），由 controller 建立，閒置時縮到 0 | 執行該 namespace 的建置與推送，保存它的 cache |
| 每次建置 | 1 個 `batch/v1` Job | clone、規劃、作為 BuildKit client 送出建置 |
| 驗證 | CRD CEL 規則加上 ValidatingAdmissionPolicy | **不跑 admission webhook server** |
| 憑證 | 由 controller 持有 | GitHub App key、**〔變更〕** 每個 namespace 的 registry 帳號、LLM API key；buildkitd 的 CA 與憑證由 controller 產生 |

**關鍵觀念**：使用者程式碼的 `RUN` 是在 **buildkitd 的 worker** 裡執行，Job Pod 只是一個薄 client。所以安全邊界與 cache 都在 buildkitd；Job Pod 不需要任何特權。**〔變更〕** 每個 namespace 一個 buildkitd，安全邊界也就落在 namespace 之間。

---

## 3. API 設計

API group 與版本：`builder.zeabur.com/v1alpha1`。

> **〔變更〕** 以下型別仍用 Go struct 標記描述 API 形狀，方便對照；實作在 `crates/zeabur-builder-api`（Rust）。差異：時間長度一律是字串（例如 `10m`）；`Resources` 只有 CPU 與記憶體；每個字串、清單與 map 都有上限，讓 CEL 成本估算有界。

### 3.1 ClusterBuilder（cluster-scoped，由管理員維護）

描述「怎麼規劃」以及「用哪個 frontend 建」。三種 builder 的規劃產物都化約成同一個形狀：**frontend、frontend options、一組檔案**。

```go
type ClusterBuilderSpec struct {
    // Type 決定規劃步驟的行為
    // +kubebuilder:validation:Enum=dockerfile;railpack;zbplan
    Type BuilderType `json:"type"`

    // Planner 為 nil 時代表不需要規劃（例如 dockerfile builder）
    Planner *PlannerSpec `json:"planner,omitempty"`

    Frontend FrontendSpec `json:"frontend"`
}

type PlannerSpec struct {
    Image string `json:"image"`
    // Deterministic=true：每次都重新跑規劃，不 pin（railpack）
    // Deterministic=false：走 pin／replan 流程（zbplan），見 §6.3
    Deterministic bool `json:"deterministic"`
    // NeedsBuildkit：規劃過程需要連 buildkitd 試建（zbplan）
    NeedsBuildkit bool `json:"needsBuildkit,omitempty"`
    // CredentialsRef：只掛進 plan 容器的 Secret（例如 LLM API key）
    // 位於 controller 所在的 namespace，由 controller 複製成 BuildRun 擁有的短期 Secret
    CredentialsRef *SecretReference `json:"credentialsRef,omitempty"`
    // +kubebuilder:default="10m"
    Timeout metav1.Duration `json:"timeout,omitempty"`
    Resources corev1.ResourceRequirements `json:"resources,omitempty"`
}

type FrontendSpec struct {
    // +kubebuilder:validation:Enum=dockerfile.v0;gateway.v0
    Type string `json:"type"`
    // gateway.v0 時必填，例如 ghcr.io/railwayapp/railpack-frontend:<版本>
    Source string `json:"source,omitempty"`
    // 預設要附加的 frontend opts
    Opts map[string]string `json:"opts,omitempty"`
}
```

CEL 規則：
- `type == 'dockerfile' ? !has(self.planner) : has(self.planner)`
- `self.frontend.type == 'gateway.v0' ? self.frontend.source != '' : true`

> **〔變更〕** 第二條規則改為雙向：`gateway.v0` 必須有 `source`，`dockerfile.v0` 不得有 `source`。

Helm chart 預設提供三個 ClusterBuilder：`dockerfile`、`railpack`、`zbplan`。**Railpack 的 planner image 版本必須和 frontend image 版本一致**。

### 3.2 Build（namespaced，由使用者維護；代表期望狀態）

最小可用範例：

```yaml
apiVersion: builder.zeabur.com/v1alpha1
kind: Build
metadata: {name: api, namespace: team-a}
spec:
  source: {git: {url: https://github.com/acme/api, ref: main}}
  builderRef: railpack
```

```go
type BuildSpec struct {
    Source     SourceSpec `json:"source"`
    BuilderRef string     `json:"builderRef"` // → ClusterBuilder 名稱

    // Output 可省略：預設為 <defaultRegistry>/<namespace>/<build-name>
    Output *OutputSpec `json:"output,omitempty"`

    // 建置時變數，一律以 BuildKit secret 方式傳入（見 §6.5）
    Env []EnvVar `json:"env,omitempty"` // value 或 valueFrom.secretKeyRef

    Triggers TriggerSpec `json:"triggers,omitempty"`
    Plan     PlanPolicy  `json:"plan,omitempty"`

    // +kubebuilder:validation:Enum=Replace;Queue
    // +kubebuilder:default=Replace
    Concurrency string `json:"concurrency,omitempty"`

    // +kubebuilder:default=10
    // +kubebuilder:validation:Minimum=1
    HistoryLimit int32 `json:"historyLimit,omitempty"`

    // 選用：建置成功後主動通知下游（見 §9）
    Notify []NotifyTarget `json:"notify,omitempty"`

    // 暫停自動觸發（手動建立 BuildRun 仍可執行）
    Suspend bool `json:"suspend,omitempty"`
}

type SourceSpec struct {
    Git struct {
        URL        string `json:"url"`
        Ref        string `json:"ref"`                  // branch 或 tag；v1 不支援萬用字元
        ContextDir string `json:"contextDir,omitempty"` // monorepo 子目錄
    } `json:"git"`
    // plain Git 模式必填；GitHub App 模式忽略
    CredentialsRef *corev1.LocalObjectReference `json:"credentialsRef,omitempty"`
}

type OutputSpec struct {
    Image string `json:"image,omitempty"`
    // Go template；可用變數：.Branch .ShortSHA .SHA .UnixTime .BuildRun
    // +kubebuilder:default="{{.Branch}}-{{.UnixTime}}-{{.ShortSHA}}"
    TagTemplate string `json:"tagTemplate,omitempty"`
    // 額外的固定 tag，例如 latest（不建議下游依賴它）
    ExtraTags []string `json:"extraTags,omitempty"`
    // 選用；預設使用 controller 持有的 robot 帳號
    PushSecretRef *corev1.LocalObjectReference `json:"pushSecretRef,omitempty"`
}

type TriggerSpec struct {
    // GitHub App 模式預設為 true
    Webhook *bool `json:"webhook,omitempty"`
    // plain Git 預設 1m；GitHub App 模式作為安全網，預設 10m
    PollInterval *metav1.Duration `json:"pollInterval,omitempty"`
    // base image digest 變更時自動重建（M3）
    OnBaseImageChange bool `json:"onBaseImageChange,omitempty"`
}

type PlanPolicy struct {
    // 只對 non-deterministic planner 有效，預設 true
    Pin *bool `json:"pin,omitempty"`
    // 新 plan 是否需要人工核准；內部場景預設 false
    RequireApproval bool `json:"requireApproval,omitempty"`
    // 用 pin 的 plan 建置失敗時，是否自動重新規劃一次；預設 true
    AutoReplanOnFailure *bool `json:"autoReplanOnFailure,omitempty"`
}

type BuildStatus struct {
    ObservedGeneration int64  `json:"observedGeneration,omitempty"`
    SourceMode         string `json:"sourceMode,omitempty"` // GitHubApp | PlainGit
    RemoteRevision     string `json:"remoteRevision,omitempty"`
    LastResolvedAt     *metav1.Time `json:"lastResolvedAt,omitempty"`
    LastWebhookAt      *metav1.Time `json:"lastWebhookAt,omitempty"`
    LatestRevision     string `json:"latestRevision,omitempty"` // 最近一次成功建置的 SHA
    LatestImage        string `json:"latestImage,omitempty"`    // repo:tag@sha256:...
    LastBuildRunRef    string `json:"lastBuildRunRef,omitempty"`
    PinnedPlanDigest   string `json:"pinnedPlanDigest,omitempty"`
    BaseImages         []string `json:"baseImages,omitempty"`   // 供 OnBaseImageChange 使用
    Conditions         []metav1.Condition `json:"conditions,omitempty"`
}
```

**Conditions**
- `SourceReady`：失敗時的 reason 為 `AppNotInstalled`、`AuthFailed`、`RefNotFound`、`CredentialsMissing`
- `BuilderReady`：reason 為 `BuilderNotFound`
- `Ready`：最近一次建置是否成功
- `PlanApprovalPending`：當 `requireApproval=true` 且有新的 plan 等待核准時出現

**Printer columns**：Builder、Source（mode）、Latest Revision（短 SHA）、Ready、Age。

**Annotation 操作介面**
- `builder.zeabur.com/rebuild: <任意值>`：值變更就以目前 HEAD 強制重建
- `builder.zeabur.com/replan: <任意值>`：值變更就丟棄 pin 的 plan，重新規劃
- `builder.zeabur.com/approve-plan: <plan digest>`：核准指定的 plan

### 3.3 BuildRun（namespaced；代表一次執行，spec 不可變）

```go
// +kubebuilder:validation:XValidation:rule="self == oldSelf",message="spec is immutable"
type BuildRunSpec struct {
    BuildRef string `json:"buildRef"`
    Revision string `json:"revision"` // 完整 40 字元 SHA；CEL 驗證格式
    // 指定沿用某份 plan（digest）；空白則依 PlanPolicy 決定
    PlanDigest string `json:"planDigest,omitempty"`
    // 為 true 時忽略 pin，強制重新規劃
    ForceReplan bool `json:"forceReplan,omitempty"`
    // 觸發來源：webhook | poll | manual | baseImage | replan
    Trigger string `json:"trigger,omitempty"`
}

type BuildRunStatus struct {
    // Pending | Planning | Building | Succeeded | Failed | Cancelled
    Phase             string `json:"phase,omitempty"`
    Reason            string `json:"reason,omitempty"` // 見下方 reason 列表
    Message           string `json:"message,omitempty"`
    JobRef            string `json:"jobRef,omitempty"`
    BuildkitdInstance string `json:"buildkitdInstance,omitempty"`
    ImageDigest       string `json:"imageDigest,omitempty"`
    Tags              []string `json:"tags,omitempty"`
    PlanDigest        string `json:"planDigest,omitempty"`
    PlanSource        string `json:"planSource,omitempty"` // pinned | generated | none
    BaseImages        []string `json:"baseImages,omitempty"`
    PhaseDurations    map[string]metav1.Duration `json:"phaseDurations,omitempty"`
    CacheStats        *CacheStats `json:"cacheStats,omitempty"` // vertices total／cached
    CheckRunID        int64 `json:"checkRunID,omitempty"`
    StartTime         *metav1.Time `json:"startTime,omitempty"`
    CompletionTime    *metav1.Time `json:"completionTime,omitempty"`
    Conditions        []metav1.Condition `json:"conditions,omitempty"`
}
```

**失敗 reason**：`SourceFailed`、`PlanFailed`、`BuildFailed`、`PushFailed`、`Timeout`、`Cancelled`、`InfraError`（buildkitd 無法連線等）

**Printer columns**：Build、Revision（短 SHA）、Phase、Reason、Duration、Age。

> **〔變更〕** printer column 只能用 JSONPath，無法算出耗時或截短 SHA。實作改為 Build、Phase、Reason、Started、Finished、Age；完整 Revision 在 `-o wide` 顯示。

BuildRun 的 ownerReference 指向 Build，Job 的 ownerReference 指向 BuildRun。

### 3.4 Plan artifact（存在 OCI registry，不是 CRD）

- 推到與 image 相同的 repository，tag 為 `plan-<內容 sha256 前 16 碼>`。
- 用 OCI artifact 格式，`artifactType: application/vnd.zeabur.builder.plan.v1`（名稱可再調整）。
- 內容：
  - `plan.json`（metadata）：builder 類型、frontend type／source／opts、planner image digest、產生時間、來源 revision、`generated|deterministic`
  - 檔案層：`Dockerfile`（zbplan 或 dockerfile builder）或 `railpack-plan.json`（railpack）
- 好處：內容定址、不佔 etcd、隨 registry GC 一起管理；使用者想「eject」時可以直接拉出 zbplan 生成的 Dockerfile。
- ~~實作建議使用 `oras-go`。~~ **〔變更〕** 使用 Rust 的 `oci-client` 0.18（支援 `artifactType`、`subject` 與 referrers API）。
- v1 也可以只對 zbplan 推 plan artifact；railpack 每次重新 prepare，artifact 只做為紀錄用途（選用）。

### 3.5 ValidatingAdmissionPolicy（取代 admission webhook）

需要 Kubernetes 1.30 以上（VAP GA）。

- **輸出路徑限制**：`Build.spec.output.image` 必須以 `<defaultRegistry>/<自己的 namespace>/` 開頭；允許管理員以 namespace label 開放外部 registry 的例外。
- **憑證使用限制**：`credentialsRef` 只能指向同 namespace 的 Secret（這本來就是 LocalObjectReference 的語意，在 controller 端也要再檢查一次）。

---

## 4. Controller 行為

~~使用 controller-runtime（依模板決定是否用 kubebuilder scaffolding）。~~

**〔變更〕** 使用 kube 4.2（`kube::runtime::Controller`）。leader election 用 `kube-leader-election` 或 `kube-lease-manager`；field indexer 以 reflector store 實作。見 ADR-9。

### 4.1 Build reconciler

觸發來源：Build 本身的變更、owned BuildRun 的變更、webhook 喚醒、poller 喚醒（**〔變更〕** 透過 `Controller::reconcile_on` 的 channel，取代 `source.Channel`／`GenericEvent`）、annotation 變更。

流程：

1. 解析 ClusterBuilder → 設定 `BuilderReady`。
2. 選出 source provider（§5）→ 設定 `status.sourceMode`。
3. **呼叫 `Resolve()` 將 ref 解析成 SHA**。這是唯一的真相來源；webhook payload 裡的 SHA **不使用**（見 ADR-3）。成功則更新 `remoteRevision` 與 `lastResolvedAt`，並設定 `SourceReady`。
4. 判斷是否需要建置：
   - `remoteRevision != latestRevision`，且沒有針對同一個 SHA 正在跑或已經成功的 BuildRun；或
   - `rebuild` annotation 有變更；或
   - base image 有變更（M3）
5. 依 `concurrency` 處理進行中的 BuildRun：
   - `Replace`：將舊的 BuildRun 標記為 `Cancelled`，刪除它的 Job，然後建立新的 BuildRun
   - `Queue`：等舊的跑完再建立新的
6. 建立 BuildRun（`generateName: <build>-`），spec 寫死 SHA 與 trigger。
7. 清除超過 `historyLimit` 的已完成 BuildRun（依完成時間排序，保留最近一次成功的那筆）。
8. 依照 pollInterval 設定 `RequeueAfter`（加 jitter）。實際的 polling 由 §4.4 的 poller 統一管理，reconciler 只負責回報下一次的時間。

`Suspend=true` 時跳過第 4 到 6 步。

### 4.2 BuildRun reconciler

狀態機：`Pending → Planning → Building → Succeeded | Failed | Cancelled`

1. **Pending**
   - 公平排程：若該 namespace 執行中的 BuildRun 數量 ≥ 上限，維持 Pending（`reason=QuotaExceeded`）。上限預設值由 controller 設定（`--max-concurrent-builds`，預設 3），可用 Namespace annotation `builder.zeabur.com/max-concurrent-builds` 覆寫。
   - ~~全域上限：所有 buildkitd 的總 slot 數。~~ **〔變更〕** 沒有全域上限：每個 namespace 有自己的 buildkitd，總容量由叢集的排程決定。
   - **〔變更〕** registry 帳號：找不到該 namespace 的推送 Secret（§8）就維持 Pending（`reason=RegistryCredentialsMissing`），也不啟動 buildkitd。
   - ~~選擇 buildkitd：使用 rendezvous hashing~~ **〔變更〕** 喚醒 buildkitd（§7.1）：確保該 namespace 的憑證、StatefulSet（replicas 1）、Service 與 NetworkPolicy 存在。Pod 還沒 Ready 就維持 Pending（`reason=BuildkitdStarting`，message 寫出等待的 Pod），每 2 秒重試。Pod 名稱寫進 `status.buildkitdInstance`。
   - **〔變更〕** 把該 namespace 的 client 憑證寫進 build namespace 的 `zeabur-builder-buildkit-client` Secret（§7.3）。
   - 若需要短期憑證（GitHub token、LLM key 副本），建立 BuildRun 擁有的 Secret。
   - 建立 Job（§6.1）。
   - GitHub App 模式下建立 check run，狀態為 `in_progress`。
2. **Planning／Building**：從 Job Pod 的 init container 狀態推導目前階段；不需要另外溝通管道。
3. **完成**：讀取 build 容器的 termination message（§6.4），寫入 digest、tags、plan、base images、各階段耗時、cache 統計。
   - 成功：更新 Build 的 `latestRevision`、`latestImage`、`pinnedPlanDigest`；發出 Kubernetes Event；送出 notify（§9）；check run 設為 `success`；記錄指標。
   - 失敗：
     - 若 `reason == BuildFailed`、`planSource == pinned`、且 `autoReplanOnFailure`：建立一個新的 BuildRun（`forceReplan=true`，`trigger=replan`）。**每個 revision 最多自動重新規劃一次**，避免無限迴圈。
     - check run 設為 `failure`，摘要中附上 reason 與 message。
4. 刪除短期 Secret（ownerReference 也會讓它被 GC，但主動刪除可以縮短暴露時間）。
5. Job 設 `ttlSecondsAfterFinished`（預設 600 秒，可設定），讓 log collector 有時間讀完（§10.1）。

Job 消失或 Pod 被驅逐時，標記 `InfraError`，**不自動重試**（v1）；使用者可以用 `rebuild` annotation 重跑。

### 4.3 Webhook 接收端

- 路徑：`POST /hooks/github`（由 controller 的 HTTP server 提供；chart 附 Service，Ingress 為選用）。
- 用 `X-Hub-Signature-256` 驗證 HMAC（以常數時間比較）。
- 用 `X-GitHub-Delivery` 去重（記憶體 LRU 即可）。
- 只處理 `push` 事件；`ping` 回 200；其他事件回 202 並忽略。
- `deleted: true` 或 `after` 為全零（刪除 branch）時忽略。
- 從 payload 取 repo URL 與 ref，**正規化**（§5.4）後，用 field indexer `spec.source.git.normalizedKey`（`<正規化 URL>#<ref>`）查出所有符合的 Build。
- 對每個符合的 Build 更新 `status.lastWebhookAt`，並把 key 送進喚醒 channel。
- **webhook 只是喚醒訊號，不攜帶真相**（ADR-3）。
- **只有 leader 處理 webhook**：非 leader 的副本收到時，回 503 讓 GitHub 記錄失敗，或者只做喚醒（喚醒本身是冪等的）。v1 只跑單一副本，所以可以先不處理這個情況，但程式碼要寫成之後擴充不必大改的形式。

### 4.4 Poller

- **以 repo 為單位去重**：把所有 Build 依正規化 URL 分組，每組每個週期只做一次 `ls-remote`，結果分給組內所有 Build（每個 Build 追的 ref 不同也沒關係，因為 `ls-remote` 一次會回傳所有 ref）。
- 每組的週期取組內 Build 最短的 pollInterval。
- 加上 ±10% 的 jitter。
- 發現某個 ref 的 SHA 和 Build 的 `remoteRevision` 不同時，喚醒該 Build。
- GitHub App 模式的 Build 也納入 poller，作為漏接 webhook 的安全網（GitHub 不會自動重送失敗的 webhook）。

---

## 5. Source providers

### 5.1 介面

```go
type SourceProvider interface {
    Name() string // "github-app" | "plain-git"
    Matches(ctx context.Context, b *Build) (bool, error)
    // 回傳該 repo 所有 ref → SHA；Resolve 用它取出單一 ref
    ListRefs(ctx context.Context, b *Build) (map[string]string, error)
    Resolve(ctx context.Context, b *Build) (sha string, err error)
    // 產生給 Job 用的 clone 憑證，寫成 BuildRun 擁有的 Secret；不需要時回傳 nil
    MintCloneCredentials(ctx context.Context, b *Build, br *BuildRun) (*corev1.Secret, error)
    // 選用：回報狀態；不支援時回傳 ErrNotSupported
    ReportStatus(ctx context.Context, b *Build, br *BuildRun) error
}
```

選擇順序：依序呼叫已設定的 GitHub App provider 的 `Matches`，若都不符合就使用 plain Git。

### 5.2 GitHub App provider

**設定**（controller 層級的 ConfigMap 加 Secret，不做成 CRD）：支援多個 App，每個 App 包含 `appID`、`privateKey`、`webhookSecret`、`baseURL`（GHES 使用）。

**App 權限（最小化）**
- Repository permissions：Contents: Read、Metadata: Read、Checks: Read & write
- 訂閱事件：`push`

**Matches**：URL 的 host 等於 App 的 host，且 App 的 installation 涵蓋這個 repo（呼叫 `GET /repos/{owner}/{repo}/installation`，結果快取 5 分鐘）。不涵蓋時回傳 false；若 Build 沒有提供 `credentialsRef`，plain Git 會失敗並給出 `AppNotInstalled`，讓使用者知道該去安裝 App。

**Resolve**：使用 installation token 呼叫 Git refs API，或對 HTTPS 做 `ls-remote`（兩者擇一，以實作簡單者為準）。

**MintCloneCredentials**：呼叫 `POST /app/installations/{id}/access_tokens`，參數 `repositories: [<repo>]` 與 `permissions: {contents: read}`，把 token 寫成 Secret（`username: x-access-token`、`password: <token>`）。token 一小時過期，而 clone 在 init container 內就完成，時間足夠。

**ReportStatus**：使用 Checks API。check run 名稱為 `build/<build-name>`，`external_id` 為 BuildRun 的 UID。summary 包含 image（tag@digest）、builder、plan 來源、各階段耗時；若有設定 log URL 樣板（§10.1），就附上 `details_url`。

~~建議使用的套件：`github.com/google/go-github` 加上 `github.com/bradleyfalzon/ghinstallation`。~~

**〔變更〕** 使用 `octocrab` 0.54.5：支援 App JWT、指定 `repositories` 與 `permissions` 的 installation token，以及 Checks API。

### 5.3 Plain Git provider

- **〔變更〕** 使用 `gix` 0.89 的 `ls-refs`（等同 `ls-remote`），controller image 不需要 git binary；但 SSH remote 需要 `ssh`（gix 透過它連線）。
- 憑證 Secret 格式（沿用 Kubernetes 慣用型別）：
  - `kubernetes.io/ssh-auth`：`ssh-privatekey`，加上**必填**的 `known_hosts`（不允許關閉 host key 檢查）
  - `kubernetes.io/basic-auth`：`username`、`password`（token）
- **Secret 必須帶 label `builder.zeabur.com/git-credentials: "true"`**；controller 的 informer cache 用這個 label 篩選 Secret，降低記憶體用量與暴露面。沒有 label 時，設定 `SourceReady=False reason=CredentialsMissing`，訊息提示要加 label。
- Job 直接掛載使用者的 Secret（同 namespace），不另外複製。
- 支援匿名存取（公開 repo，不提供 `credentialsRef`）。

### 5.4 URL 正規化

把下列寫法都轉成同一個 key：`https://GitHub.com/Acme/api.git`、`git@github.com:acme/api.git`、`ssh://git@github.com/acme/api`。

規則：host 轉小寫；去掉 `.git` 尾綴與結尾的 `/`；SSH 的 scp 寫法轉成路徑；去掉使用者名稱。GitHub 的 owner／repo 不分大小寫，所以也轉成小寫；其他 host 保留路徑的大小寫。要寫完整的表格驅動單元測試。

**〔變更〕** 另外：去掉預設 port（22、80、443、9418）但保留其他 port；去掉 query 與 fragment；拒絕本機路徑、`file://` 與 remote helper 語法（`ext::`、`fd::`）。已實作於 `zeabur-builder-core::repo`。

---

## 6. Builder 與 Job

### 6.1 Job Pod 結構

```
initContainers:
  fetch  (zeabur-builder-job image)   clone 指定 SHA 到 /workspace/src（shallow，depth=1，用 SHA fetch）
  plan   (planner image)              依 builder 執行；dockerfile builder 跳過這一步
containers:
  build  (zeabur-builder-job image)   zeabur-builder-job -- buildctl build …：solve、push、寫 termination message
volumes:
  workspace (emptyDir)          /workspace/{src,plan,out}
  git-credentials (Secret)      只掛給 fetch
  planner-credentials (Secret)  只掛給 plan
  buildkit-client-tls (Secret)  掛給 build（若 planner.needsBuildkit 也掛給 plan）
  registry-auth (Secret)        只掛給 build
```

**Job 設定**
- `backoffLimit: 0`、`restartPolicy: Never`：重試語意由 controller 決定。
- `activeDeadlineSeconds`：planner timeout 加上 build timeout（預設 60 分鐘，可設定）。
- `ttlSecondsAfterFinished`：預設 600。
- `terminationMessagePolicy: File`，路徑為預設的 `/dev/termination-log`。

**〔變更〕** `buildkit-client-tls` 是該 namespace 自己的 client 憑證，只有該 namespace 的 buildkitd 接受；`BUILDKIT_HOST` 指向 `buildkitd-<namespace>-0`。

**Pod 安全設定**：`runAsNonRoot: true`、`readOnlyRootFilesystem: true`（`/workspace` 與 `/tmp` 使用 emptyDir）、drop 所有 capabilities、`seccompProfile: RuntimeDefault`、`automountServiceAccountToken: false`。

**〔變更〕** 補充：
- `zeabur-builder-job` image 以 `moby/buildkit:v0.34.1`（Alpine 版，內含 `buildctl`、`git`、`openssh`）為基礎，加上靜態連結的 `zeabur-builder-job`；Pod 以 UID 1000 執行。
- kubelet 會展開 `command`／`args` 裡的 `$(VAR)`，所以使用者提供的值（URL、路徑、frontend opts）一律把 `$` 寫成 `$$`；傳給 shell 時只當位置參數（`"$1"`），不拼進腳本。
- `fetch` 設定 `GIT_ALLOW_PROTOCOL=https:http:ssh:git`，並在建立 Job 前檢查 URL 與 SHA。
- Build 的變數以 `ZB_SECRET_<NAME>` 傳進 build 容器，避免名為 `PATH`、`BUILDKIT_HOST` 的變數影響 `buildctl`。
- image 的 `org.opencontainers.image.source` 使用正規化後的 repo key，而不是原始 URL（URL 可能含 token）。

**Labels**（log collector 會用到，§10.1）：`builder.zeabur.com/build`、`builder.zeabur.com/buildrun`、`builder.zeabur.com/builder`，加上 `app.kubernetes.io/managed-by: zeabur-builder`。
**Annotations**：`builder.zeabur.com/revision`。

### 6.2 各 builder 的規劃步驟

| builder | plan 容器做的事 | 產物（位於 `/workspace/plan`） | frontend |
|---|---|---|---|
| dockerfile | 無 | 使用 repo 內的 Dockerfile（路徑可在 Build 設定，預設 `<contextDir>/Dockerfile`） | `dockerfile.v0` |
| railpack | `railpack prepare /workspace/src/<ctx> --plan-out /workspace/plan/railpack-plan.json --info-out /workspace/plan/railpack-info.json`（若有 Env，名稱用 `--env` 傳入） | `railpack-plan.json` | `gateway.v0`，`source=ghcr.io/railwayapp/railpack-frontend` |
| zbplan | 有 pin 的 plan 時：從 registry 拉 plan artifact；否則執行 `zbplan --buildkit-addr <addr> --context-dir /workspace/src/<ctx>`（環境變數 `ZBPLAN_ANTHROPIC_API_KEY`），擷取生成的 Dockerfile | `Dockerfile` | `dockerfile.v0`（≥ 1.10，因為需要 secret env mount） |

Railpack 參考：<https://railpack.com/guides/running-railpack-in-production/>、<https://railpack.com/reference/frontend>

~~`[待驗證]` zbplan 的輸出、exit code、輪數與 token、LLM 設定、授權~~ **〔已驗證〕**（細節見 feasibility.md）：
- Dockerfile 輸出到 stdout，log 在 stderr；plan 容器以 `zbplan … > /workspace/plan/Dockerfile` 擷取。
- 成功 exit 0，任何失敗 exit 1。
- CLI 不提供輪數或 token 用量；輪數只在 Go library 的 `Result.Attempts`，token 用量完全沒有。§10.2 的 token 指標需要上游支援。
- `ZBPLAN_MODEL=claude|openai`，以及 `ZBPLAN_ANTHROPIC_*`、`ZBPLAN_OPENAI_*`（`_API_KEY`、`_MODEL`、`_BASE_URL`）；OpenAI 相容的 gateway 可用。
- 以獨立 binary 執行，不以 library 引用；在 planner image 附上授權與原始碼連結即可。
- **新發現**：zbplan 連 buildkitd 時不支援 TLS（`client.New(ctx, addr)`），無法連到要求 client 憑證的 buildkitd，需要上游加上 TLS 參數。

Railpack frontend 也已驗證：frontend 只讀帶 `build-arg:` 前綴的 opts（見 §6.5）。

所有 planner image 由我們包裝（Dockerfile 放在 repo 的 `images/` 底下），並 pin 上游版本。

### 6.3 zbplan 的 pin 與 replan 流程

1. 若 BuildRun 指定 `planDigest`，或 Build 有 `pinnedPlanDigest` 且沒有 `forceReplan`：**直接拉取該 plan，完全不呼叫 LLM**（`planSource=pinned`）。
2. 否則執行 zbplan → 推 plan artifact → `planSource=generated`。
   - 若 `requireApproval=true`：Job 只做到規劃就結束（build 容器偵測到這個模式後只推 plan、不建置），BuildRun 以 `Succeeded` 加上 condition `AwaitingApproval` 結束，Build 設定 `PlanApprovalPending`。核准 annotation 出現後，再以該 planDigest 建立新的 BuildRun。
3. 用 pin 的 plan 建置失敗（`BuildFailed`）時，依 §4.2 自動重新規劃一次。
4. 建置成功後，Build 的 `pinnedPlanDigest` 更新為這次的 plan。

zbplan 在規劃時就已經在同一台 buildkitd 上試建過，所以接下來的正式建置大多會命中 cache，額外成本很低。**plan 容器與 build 容器必須連到同一個 buildkitd instance**。

### 6.4 `zeabur-builder-job`（**〔變更〕** Rust 包裝程式加上原生 `buildctl`）

~~同一個 image 提供兩個子指令：`fetch`（go-git）與 `build`（BuildKit Go client，不包裝 buildctl CLI）。~~

**〔變更〕** 見 ADR-9。同一個 image（`moby/buildkit` 加上 `zeabur-builder-job`）提供：

- `fetch`：init container 以 `git`（image 內建）執行：`git fetch --depth=1 -- <url> <sha>`，checkout 後驗證 HEAD 等於指定的 SHA；支援 SSH（必填 `known_hosts`）與 basic auth。
- `build`：`zeabur-builder-job [--secrets-hash-env NAME …] -- buildctl … build …`。`buildctl` 參數由 controller 的 `zeabur-builder-jobspec`（純函式、golden test）組出；包裝程式只負責執行它、讀取 `--progress rawjson`、重新輸出人看的進度、統計 cache、讀 `--metadata-file` 的 `containerimage.digest`、寫 termination message。解析邏輯在 `zeabur-builder-core`（`progress`、`result`），已有測試。

`build` 的職責（**〔變更〕** 對應到 `buildctl` 參數）：
1. 讀 plan metadata，決定 frontend 與 opts。
2. 組出 solve 參數：
   - local mounts：`--local context=/workspace/src/<ctx>`、`--local dockerfile=/workspace/plan`
   - secrets：Build 的 Env（§6.5），`--secret id=<NAME>,env=ZB_SECRET_<NAME>`
   - cache：`--import-cache` 與 `--export-cache type=registry,ref=<image>:buildcache,mode=max,ignore-error=true` 作為後援；主要依靠 buildkitd 本地的 cache
   - attestations：provenance 與 SBOM（`--opt attest:provenance=…`、`--opt attest:sbom=`）。**〔變更〕** 預設 `mode=min`：`mode=max` 會記錄 build arg 的值
   - output：`--output type=image,"name=<image>:<tag>,<image>:<extra>",push=true,oci-mediatypes=true`
   - OCI annotations：以 `annotation.org.opencontainers.image.*` 寫在 output 上；`created` 留到 M3
3. registry 認證：`DOCKER_CONFIG` 指向掛載的 docker config.json；`buildctl` 透過 session 的 auth provider 提供憑證，**推送由 buildkitd 執行，但憑證不會進入 `RUN` 的容器**（依 session 設計推論，尚未實測）。
4. mTLS：`--tlsdir`（`ca.pem`、`cert.pem`、`key.pem`）。
5. 進度輸出：以人看的 plain 格式寫到 stdout，同時統計 vertex 總數與命中 cache 的數量。
6. 推送 plan artifact（zbplan 必推；railpack 選推）。**〔變更〕** 由包裝程式以 `oci-client` 推送。
7. 從 provenance 的 materials 取出 base image 列表（`name@digest`），最多 10 筆。
8. 寫入 termination message（**必須小於 4096 bytes**）：

```json
{
  "result": "succeeded",
  "reason": "",
  "message": "",
  "digest": "sha256:...",
  "tags": ["main-1761058800-3e1a9c2"],
  "planDigest": "sha256:...",
  "planSource": "pinned",
  "baseImages": ["docker.io/library/node:24-alpine@sha256:..."],
  "phases": {"fetch": 3.1, "plan": 0.4, "build": 82.0, "push": 6.2},
  "cache": {"total": 41, "cached": 37}
}
```

   fetch 與 plan 的耗時由各自的 init container 寫進 `/workspace/out/*.json`，build 容器最後彙整。失敗時也必須寫入 `result`、`reason`、`message`（message 截斷到 1 KB）。
9. stdout 的最後一行輸出同樣的 JSON 摘要（方便 log 查詢）。

**Tag 渲染**：`.Branch` 中的 `/` 轉成 `-`，並截斷以符合 OCI tag 的規則（`[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`）；`.UnixTime` 使用 BuildRun 建立的時間，確保同一個 BuildRun 重試時 tag 不變。

### 6.5 建置變數與 secret

- Build 的 `env` 一律以 BuildKit **secret** 傳入，不使用 build arg，避免值出現在 image metadata。
- **Railpack**：名稱用 `railpack prepare --env` 傳入（寫進 plan 的只有名稱）；建置時以 secret 傳值，並傳入 ~~`secrets-hash`~~ **〔變更〕** `build-arg:secrets-hash` opt（所有值的 sha256，由包裝程式計算），讓值變更時 cache 會失效。Railpack frontend 只讀 `build-arg:` 前綴的 opts，沒有前綴的會被默默忽略。`build-arg:cache-key` 設為 `<namespace>/<build>`。
- **Dockerfile／zbplan**：以 secret env mount 的方式提供給 `RUN`。
- ~~`[待驗證]`~~ **〔已驗證〕zbplan 的 cache 地雷**：zbplan 的 `pkg/buildenv` 在 secret ID 中使用以 process 為 key 的 digest，新的 process 會使用新的 namespace，導致每個 Job 都 cache miss。對策：在我們的 build binary 中自己處理 secret 注入，namespace key 使用 per-Build 的穩定值（例如用 controller 持有的金鑰對 Build UID 做 HMAC）。先讀 `pkg/buildenv` 的 API，看能否從外部傳入 key；不行的話就自己實作 Railpack 那種「名稱寫進 plan、值的雜湊當 cache key」的機制。
  - **結果**：無法傳入。`cacheKey` 是每個 process 以 `crypto/rand` 產生的私有值，secret ID 為 `HMAC(cacheKey, name\0value)`。zbplan 輸出的 Dockerfile 不含變數注入，注入發生在建置當下。CLI 不傳任何變數，所以 zbplan 自己的試建不受影響。
  - **對策**：請 zbplan 上游提供可傳入 key 的選項，以及單獨執行注入的方式（例如 `zbplan prepare-env`），讓 plan 容器每次建置都以 per-Build 的穩定 key 處理 pin 的 Dockerfile。zbplan 是 Zeabur 自己的專案，改動小；只影響 M4。
- 文件中要提醒使用者：不要把 secret 寫進 cache mount 目錄；在 Dockerfile 中以 `ENV X=$SECRET` 引用會讓值變成公開的 build arg。

---

## 7. buildkitd：每個 namespace 一個

**〔變更〕** 原本是全叢集共用一個 buildkitd 池，以 rendezvous hashing 依 repo 分配（見 ADR-8）。現在每個 build namespace 有自己的 buildkitd，由 controller 建立與縮放；不再有路由，也不再跨 namespace 共用 cache。

### 7.1 部署

- 某個 namespace 第一次有 BuildRun 要開始時，controller 在 builder 自己的 namespace（PoC 是 `builder-system`）以 server-side apply 建立：
  - StatefulSet `buildkitd-<namespace>`：1 個 replica，`moby/buildkit:v0.34.1`（**rootful**），pin 版本
  - headless Service `buildkitd-<namespace>`；Job 連 `buildkitd-<namespace>-0.buildkitd-<namespace>.<builder namespace>.svc:1234`
  - ingress NetworkPolicy（§7.2）與憑證 Secret `buildkitd-<namespace>-tls`（§7.3）
  - namespace 名稱太長時（StatefulSet 名稱上限 52 字元，因為 `controller-revision-hash` label），改用前 33 個字元加 SHA-256 的前 8 個 hex 字元。
- cache 是 `volumeClaimTemplates` 建立的 PVC，掛在 `/var/lib/buildkit`，大小預設 20Gi（`--buildkitd-cache-size`），storage class 可設定。`persistentVolumeClaimRetentionPolicy` 是 `whenScaled: Retain`、`whenDeleted: Delete`：縮到 0 時 cache 留著，StatefulSet 刪除時一起刪。
- **閒置縮到 0**：controller 每 30 秒檢查一次。namespace 沒有未完成的 BuildRun，且最近一次 BuildRun 建立或完成已超過 `--buildkitd-idle`（預設 600 秒），就經由 `scale` subresource 把 replicas 設為 0；下一個 BuildRun 再 apply 回 1。冷啟動在測試叢集上約 5 到 8 秒（§16）。
- **namespace 刪除**：同一個檢查發現 namespace 已不存在時，刪除它的 StatefulSet、Service、NetworkPolicy 與憑證 Secret；PVC 依保留政策一起刪除。
- 所有 buildkitd 共用一個 ConfigMap 裡的 `buildkitd.toml`（`--buildkitd-config`）：
  - GC 政策：~~`keepBytes` 設為 PVC 大小的 80% 左右~~ **〔變更〕** BuildKit v0.34 以 `reservedSpace`、`maxUsedSpace`、`minFreeSpace`、`keepDuration` 設定；`keepBytes` 是舊名稱。設 `maxUsedSpace = "80%"`、`minFreeSpace = "20%"`，在 kubelet 的驅逐門檻之前先清
  - **〔新增〕** `insecure-entitlements = []`：`RUN --network=host` 與 `--security=insecure` 一律拒絕
  - **〔新增〕** `[worker.oci] networkMode = "bridge"`（§7.2）
  - **〔新增〕** `[frontend."gateway.v0"] allowedRepositories`：只允許列出的 gateway frontend（`docker.io/docker/dockerfile`、`docker.io/docker/dockerfile-upstream`、Railpack 的 frontend）。Dockerfile 開頭的 `# syntax=` 也受這個限制；新增 ClusterBuilder 時要把它的 frontend 加進來
  - `max-parallelism`：可設定
  - 設定改了以後，要等各 namespace 的 buildkitd 下次啟動才生效（例如閒置縮到 0 之後）
- 資源：requests 預設 100m CPU、256Mi 記憶體（`--buildkitd-cpu-request`、`--buildkitd-memory-request`），limit 選用。閒置時實際約用 22 MiB anonymous memory（§16）。
- readiness probe：**〔已驗證〕** 使用 exec probe `buildctl --addr unix:///run/buildkit/buildkitd.sock debug workers`。它走本機 socket，不受 TCP 的 mTLS 影響；gRPC health probe 無法出示 client 憑證。上游 Kubernetes 範例也這樣做。

### 7.2 隔離

- Kubernetes 1.33 以上：設定 `hostUsers: false`（user namespace），搭配 rootful buildkitd。注意 **rootless buildkitd 不能與 user namespace 併用**。低於 1.33 時，以 `--buildkitd-host-users=true` 退回單純的 `privileged`，並在文件中說明風險。
  - **〔已驗證〕** 1.33 起為 beta 且預設開啟，1.36 GA。需要 Linux ≥ 6.3、containerd ≥ 2.0、runc ≥ 1.2，以及支援 idmapped mount 的檔案系統（NFS 不行）。上游有 `statefulset.userns.yaml` 範例。
- **〔新增〕** sandbox runtime：`--buildkitd-runtime-class` 設定 buildkitd Pod 的 `runtimeClassName`，預設不設。替不受信任的客戶建置時，指定 gVisor 或 Kata 的 RuntimeClass（§1.3）。`privileged` 的 buildkitd 在這些 runtime 下能否運作、要多少額外資源，尚未驗證（§16）。
- 獨立 node pool：用 taint 與 toleration 區隔（chart 中為選用設定）。
- **〔新增〕** `RUN` 的網路：`networkMode = "bridge"` 讓每個 `RUN` 有自己的網路 namespace，經 BuildKit 內建的 CNI bridge 外出，來源位址被 masquerade 成 buildkitd Pod 的 IP，所以 buildkitd Pod 的 egress NetworkPolicy 同樣管得到 `RUN`。不用 host network：那樣的 `RUN` 與 buildkitd 共用網路，Pod 連得到的它都連得到，包括 buildkitd 自己的 gRPC port。**〔已驗證〕** 在 `hostUsers: false` 的 Pod 中可用：`RUN` 拿到 `10.10.0.0/16` 的位址，worker 標籤為 `network:cni`。
- NetworkPolicy：
  - ingress：~~只允許 controller namespace 中帶 build Job label 的 Pod 連 1234 port~~ **〔變更〕** 每個 namespace 一份，由 controller 建立：只允許該 build namespace 的 Pod 連 1234 port。其他 namespace 的 Pod 連不上；連得上的還要有該 namespace 的 client 憑證（§7.3）。
  - egress：~~允許所有外部連線，但封鎖 cloud metadata endpoint~~ **〔變更〕** 所有 buildkitd 共用一份，由部署提供：允許 kube-dns 的 53 port、registry 的 Pod，以及 `0.0.0.0/0` 中除了 `10.0.0.0/8`、`100.64.0.0/10`、`127.0.0.0/8`、`169.254.0.0/16`、`172.16.0.0/12`、`192.168.0.0/16` 以外的位址。私有網段涵蓋常見的 Pod、Service、node 網段與 cloud metadata endpoint；叢集若用公有網段，要自己加進 `except`。目前只處理 IPv4（`fd00:ec2::254` 等 IPv6 位址待補）。
  - `RUN` 仍可經 kube-dns 解析叢集內的名稱，只是連不上。
  - 需要叢集的 CNI 執行 NetworkPolicy（k3s 內建；Flannel 單獨使用時不會執行）。

### 7.3 mTLS

- ~~v1 由 controller 啟動時自行產生 CA 與憑證並存成 Secret~~ **〔變更〕** 每個 namespace 有自己的 CA。controller 第一次喚醒該 namespace 的 buildkitd 時產生 CA、server 憑證（SAN 為該 namespace 的 Service 與 Pod 名稱）與 client 憑證，以一次 create 寫進 `buildkitd-<namespace>-tls`。兩個 BuildRun 同時開始時，憑證也一定出自同一個 CA。CA 金鑰不保存。
- buildkitd 以 `--tlscacert` 驗證 client 憑證（`RequireAndVerifyClientCert`），不檢查名稱，所以 CA 就是邊界：A 的 client 憑證連 B 的 buildkitd，會在 TLS 握手時被拒絕。
- client 憑證複製到 build namespace 的 `zeabur-builder-buildkit-client`，由 Job 掛載。能讀這個 Secret 的人可以直接用 `buildctl` 連自己 namespace 的 buildkitd；這沒有越過 namespace 的邊界。
- 自動輪替尚未實作；cert-manager 為選用。

### 7.4 ~~路由：rendezvous hashing~~

**〔變更〕** 刪除。每個 namespace 只有一個 buildkitd，不需要路由；`NoBuildkitdCapacity` 改為 `BuildkitdStarting`。

---

## 8. Registry 與憑證

- 預設 registry 由 controller 設定（`defaultRegistry`）。~~推送使用 controller 持有的 robot 帳號~~ **〔變更〕** 每個 build namespace 有自己的推送帳號，只能推到 `<defaultRegistry>/<namespace>/**`：
  - 管理員在 controller 的 namespace 建立 docker config.json Secret `<prefix><namespace>`（`--registry-auth-prefix`，PoC 為 `registry-push-`）。
  - 找不到就讓 BuildRun 維持 Pending（`RegistryCredentialsMissing`）。沒設定 prefix 時以匿名推送，只適合不需驗證的 registry。
  - controller 不產生帳號，也不呼叫 registry 的 API；帳號的範圍由 registry 自己的 ACL 決定。之後可以由 controller 自動產生（例如寫 Zot 的 htpasswd），但目前不做。
- 每次建置時，controller 把該 namespace 的帳號複製成 BuildRun 擁有的短期 Secret 掛給 build 容器。
- Build 可以用 `pushSecretRef` 改用自己的憑證（例如推到外部 registry）。
- 內部 registry 建議使用 Zot 或 distribution；Harbor 也可以，但比較重。**〔新增〕** 以 Zot（v2.1.22 驗證過）為例，每個 namespace 一個 htpasswd 使用者，ACL 如下；任何人可以讀，只有同名使用者能寫：

  ```json
  {
    "http": {
      "auth": {"htpasswd": {"path": "/etc/zot/auth/htpasswd"}},
      "accessControl": {
        "repositories": {
          "**": {"anonymousPolicy": ["read"]},
          "team-a/**": {
            "policies": [{"users": ["team-a"], "actions": ["read", "create", "update", "delete"]}],
            "anonymousPolicy": ["read"]
          }
        }
      }
    }
  }
  ```

  htpasswd 掛成 Secret volume（不要用 `subPath`），Zot 偵測到檔案變更會自己重新載入，新增帳號不必重啟；kubelet 同步 Secret 約需一分鐘。新增 namespace 時 ACL 要加一筆，這需要重新載入設定。匿名讀取讓 node 拉 image 不需要 pull secret；若 image 本身也要保密，改成只允許同名使用者讀。
- 維運文件需要涵蓋：
  - node 上的 containerd 要信任內部 registry 的 TLS（`/etc/containerd/certs.d/<host>/hosts.toml`）
  - 每次 commit 都會產生一個 tag，需要設定 registry GC 與保留政策
  - `buildcache` tag 與 `plan-*` tag 的保留策略

---

## 9. 與 rollout 工具整合（我們不實作 rollout）

我們提供的輸出：
1. `Build.status.latestImage`（`repo:tag@digest`）
2. Kubernetes Event：`BuildSucceeded` 與 `BuildFailed`
3. 可排序且不可變的 tag：`<branch>-<unixtime>-<shortsha>`
4. 選用的主動通知（`spec.notify`），建置成功後送出：

```go
type NotifyTarget struct {
    // +kubebuilder:validation:Enum=keel;flux-receiver;generic
    Type      string `json:"type"`
    URL       string `json:"url"`
    SecretRef *corev1.LocalObjectReference `json:"secretRef,omitempty"` // token 或 HMAC key
}
```

- `keel`：送 Keel 的 native webhook。**〔已驗證〕** `POST /v1/webhooks/native`（port 9300），body 為 `{"name": "<image>", "tag": "<tag>"}`。
- `flux-receiver`：觸發 Flux 的 `Receiver`，讓 ImageRepository 立刻重新掃描。**〔已驗證〕** 使用 `generic-hmac` 類型，header 為 `X-Signature: sha256=<hex>`，`resources` 列出 `image.toolkit.fluxcd.io/v1` 的 `ImageRepository`。
- `generic`：POST 一份 JSON（build、namespace、revision、image、digest、tags），若有提供 key 就加上 HMAC 簽章 header。
- 通知失敗**不影響** BuildRun 的結果，只記錄 Event 與 `notify_errors_total` 指標；最多重試 3 次（指數退避）。

文件需附上整合範例（放在 `docs/integrations/`）：
- **Flux Image Automation**：`ImagePolicy` 搭配 `filterTags`，用正規表示式 `^main-(?P<ts>[0-9]+)-[a-f0-9]+$` 抽出 `ts`，再用 `numerical` 排序取最新
- **Flux Operator**：`ResourceSetInputProvider`（`type: OCIArtifactTag`）做 Gitless 的 image automation
- **Keel**：workload annotation 加上 notify 設定
- **Argo CD Image Updater**、**Kargo**：簡短說明即可

---

## 10. 可觀測性

### 10.1 Log（不收集、不保存）

我們只負責讓使用者的 collector（Fluent Bit、Vector、Promtail、OTel Collector 等）收得到、找得到：

- Job 的 `ttlSecondsAfterFinished` 預設 600 秒：collector 是 tail node 上的檔案，Pod 刪太快會遺漏最後幾行。
- Job Pod 帶齊 label 與 annotation（§6.1）。
- 使用者程式碼 `RUN` 的輸出會經由 progress stream 回到 build 容器的 stdout，所以全部都在 Job Pod 的 log 裡。
- controller 使用結構化 JSON log（**〔變更〕** `tracing`），每筆 log 帶 `build`、`buildrun`、`namespace`。
- **Log URL 樣板**（controller 設定，選用），用於 GitHub check run 的 `details_url` 與 BuildRun 的 annotation，例如：
  `https://grafana.internal/explore?...{buildrun="{{.BuildRun}}",namespace="{{.Namespace}}"}`

### 10.2 Prometheus 指標

**〔變更〕** kube-rs 沒有 controller-runtime 那樣的內建指標；reconcile 次數、耗時與錯誤由我們自己輸出（`prometheus-client`）。另外加上以下指標（prefix `zeabur_builder_`）：

| 指標 | 類型 | labels |
|---|---|---|
| `buildrun_total` | counter | namespace, build, builder, result, reason |
| `buildrun_phase_duration_seconds` | histogram | builder, phase（fetch／plan／build／push）, result |
| `buildrun_queue_wait_seconds` | histogram | namespace |
| `build_last_success_timestamp_seconds` | gauge | namespace, build |
| `buildruns_active` | gauge | namespace |
| `buildruns_pending` | gauge | namespace, reason |
| `buildrun_vertices_total` | counter | builder, cached（true／false） |
| ~~`buildrun_route_fallback_total`~~ **〔變更〕** `buildkitd_start_seconds` | histogram | — |
| `plan_total` | counter | builder, source（pinned／generated）, result |
| `plan_duration_seconds` | histogram | builder |
| `webhook_received_total` | counter | event, result（accepted／ignored／bad_signature／duplicate） |
| `git_resolve_duration_seconds` | histogram | mode |
| `git_resolve_errors_total` | counter | mode, reason |
| `notify_errors_total` | counter | type |

**不准當 label 的欄位**：commit SHA、BuildRun 名稱、image digest、tag（會造成 cardinality 爆炸）。

### 10.3 打包

- `ServiceMonitor`（選用，給有裝 Prometheus Operator 的環境）。
- 範例 `PrometheusRule`：
  - 連續 N 次建置失敗
  - `build_last_success_timestamp_seconds` 太久沒更新
  - `buildruns_pending` 持續偏高
  - `webhook_received_total{result="bad_signature"}` 增加
  - buildkitd 的 cache PVC 快滿（使用 kubelet 的 `kubelet_volume_stats_used_bytes`）

---

## 11. Controller 設定

用一個 ConfigMap（YAML）加上 Secret，啟動時載入；在 Helm values 中暴露：

```yaml
defaultRegistry: registry.internal
github:
  apps:
    - name: github-com
      baseURL: https://api.github.com
      appID: 12345
      secretRef: github-app-github-com      # privateKey、webhookSecret
buildkitd:                                  # 〔變更〕 每個 namespace 一個
  image: docker.io/moby/buildkit:v0.34.1
  config: buildkitd                         # ConfigMap，內含 buildkitd.toml
  cacheSize: 20Gi
  storageClass: ""
  runtimeClass: ""                          # 選用，例如 gvisor
  hostUsers: false
  resources: {cpuRequest: 100m, memoryRequest: 256Mi, memoryLimit: ""}
  idleAfter: 600s
registryAuthPrefix: registry-push-          # 〔變更〕 每個 namespace 的推送帳號 Secret
fairness:
  defaultMaxConcurrentPerNamespace: 3
job:
  ttlSecondsAfterFinished: 600
  defaultTimeout: 60m
  buildImage: ghcr.io/zeabur/builder-job:<version>
logs:
  urlTemplate: ""                            # 選用
metrics:
  prefix: zeabur_builder
```

---

## 12. RBAC（controller）

- 自家 CRD：完整權限（包含 status）。
- `batch/jobs`：create、get、list、watch、delete。
- `pods`：get、list、watch（只用來讀取狀態與 termination message）。**〔變更〕** 不需要 `pods/log`：termination message 在 Pod status 裡。
- `secrets`：
  - 叢集範圍 get、list、watch（informer 只快取帶 `builder.zeabur.com/git-credentials` label 的 Secret）
  - 在 build namespace 中 create、delete（短期 Secret）
- `events`：create、patch。
- **〔新增〕** `namespaces`：get（清掉已刪除 namespace 的 buildkitd）。
- **〔新增〕** 只在 builder 的 namespace：`statefulsets` get、list、watch、create、patch、delete；`statefulsets/scale` patch；`services`、`networkpolicies` get、create、patch、delete。
- `coordination.k8s.io/leases`：leader election。
- 不需要 admissionregistration 權限（VAP 由 chart 安裝）。

Job Pod 使用的 ServiceAccount 不需要任何權限（`automountServiceAccountToken: false`）。

---

## 13. 目錄結構

**〔變更〕** 沿用 Zeabur Identity 的 Cargo workspace；crate 分層（`scripts/check-levels.sh` 檢查）：

```
crates/zeabur-builder-core/     （第 0 層，純函式）URL 正規化、tag 渲染、buildkitd 命名、termination message、progress 統計
crates/zeabur-builder-api/      （第 1 層，純函式）CRD 型別、CEL 規則、deploy/crds.json
crates/zeabur-builder-jobspec/  （第 2 層，純函式）Job 與 buildkitd 物件組裝、golden test
crates/zeabur-builder/          （第 3 層）controller binary；之後加入 reconciler、source、webhook、notify、metrics
crates/zeabur-builder-job/      （之後）Job 內的包裝程式
deploy/                         CRD、RBAC、VAP、buildkitd StatefulSet、NetworkPolicy
images/                         planner image 與 job image 的 Dockerfile
docs/                           使用者文件、integrations/、operations/
```

原始計畫的建議（Go 專案）：

```
api/v1alpha1/                 CRD types、CEL 規則、deepcopy
cmd/controller/               controller 進入點
cmd/build/                    zeabur-builder-job binary（fetch／build 子指令）
internal/controller/          build_controller.go、buildrun_controller.go
internal/source/              provider 介面、githubapp/、plaingit/、normalize.go
internal/scheduler/           公平排程、rendezvous hashing
internal/jobspec/             Job 組裝（純函式，方便測試）
internal/plan/                plan artifact 讀寫（oras-go）
internal/buildkit/            BuildKit client 包裝、progress 統計
internal/webhook/             GitHub webhook handler
internal/notify/              keel／flux-receiver／generic
internal/metrics/
internal/config/
images/                       planner image 的 Dockerfile（railpack、zbplan）
config/ 或 charts/zeabur-builder/     CRD、RBAC、VAP、buildkitd StatefulSet、NetworkPolicy、ServiceMonitor、PrometheusRule
docs/                         使用者文件、integrations/、operations/
test/e2e/
```

---

## 14. 測試策略

- **單元測試**：URL 正規化、tag 渲染、buildkitd 命名、Job 與 buildkitd 組裝（golden file）、termination message 解析、webhook 簽章驗證。
- **Controller 測試**：~~使用 envtest~~ **〔變更〕** Rust 沒有 envtest。狀態機的決策寫成純函式（輸入：物件與觀察到的 Job／Pod；輸出：要做的動作），以單元測試涵蓋 Replace／Queue、歷史清理、自動 replan 只發生一次、Pending 排程。API server 的行為則在 k3d 或 kind 上以 `#[ignore]` 測試驗證，做法同 Zeabur Identity。
- **整合測試**（kind）：
  - 部署 buildkitd、Zot registry、controller
  - 用本地 Git server（例如 Gitea 容器或 `git daemon`）測試 plain Git polling
  - 範例 repo：Dockerfile（Go）、Railpack（Node）
  - 驗證：image 推上去了、digest 寫入 status、指標有數值
- **zbplan e2e**：只有在環境變數提供 LLM API key 時才執行（CI 預設略過）。
- **GitHub App**：webhook handler 使用錄製的 payload 測試；真實 App 的驗證以手動測試清單進行。

---

## 15. 里程碑

每個里程碑都要可以獨立展示並通過驗收。

### M0：骨架與 Dockerfile 路徑
- 依模板建立專案、CRD types（三個 kind）、CEL 規則、printer columns **〔已完成〕**
- buildkitd StatefulSet、mTLS 憑證產生
- `zeabur-builder-job` 的 `fetch` 與 `build`（Dockerfile frontend、推送、termination message）**〔Job 組裝與解析邏輯已完成；包裝程式與 image 未完成〕**
- BuildRun reconciler：手動建立 BuildRun（Revision 填 SHA）→ Job → status 寫入 digest
- 只支援匿名 Git 與預設 registry
- ~~rendezvous hashing 路由~~ **〔變更〕** 每個 namespace 一個 buildkitd，閒置縮到 0 **〔已完成〕**
- 基本指標：`buildrun_total`、`buildrun_phase_duration_seconds`

**驗收**：在 kind 上手動建立 BuildRun，image 成功推到 Zot，`kubectl get buildrun` 顯示 Succeeded 與 digest；第二次建置相同 SHA 時，cache 命中率顯著提高。

### M1：Railpack
- railpack planner image、ClusterBuilder `railpack`
- gateway frontend 的 solve 路徑
- Env 與 secrets-hash

**驗收**：一個沒有 Dockerfile 的 Node 專案可以建置成功；修改 secret 值會讓相關 layer 重建。

### M2：Build reconciler 與觸發
- Build reconciler（ref 解析、Replace／Queue、歷史清理、`rebuild` annotation、Suspend）
- plain Git provider（SSH、basic auth、label 篩選）與 poller（repo 去重、jitter）
- GitHub App provider：installation token、webhook handler、Checks 回報
- 公平排程（namespace 上限）
- VAP、log URL 樣板
- 其餘指標

**驗收**：push 到 GitHub 後數秒內開始建置，commit 上出現 check run；plain Git repo 在一個 polling 週期內被建置；連續兩次 push 在 Replace 模式下只會留下最後一次的建置。

### M3：輸出與整合
- attestation（provenance、SBOM）、OCI annotations
- `spec.notify`（keel、flux-receiver、generic）
- `OnBaseImageChange`：定期（例如每 6 小時）解析 Build 的 base image digest，有變更就觸發重建
- `docs/integrations/` 範例

**驗收**：搭配 Flux ImagePolicy 與 Keel 各完成一次端到端的自動 rollout。

### M4：zbplan
- zbplan planner image 與 wrapper（輸出擷取已驗證：stdout）
- **〔變更〕** 前置條件：zbplan 上游支援 buildkitd mTLS，以及可傳入 key 的變數注入（§6.5）
- plan artifact 的讀寫
- pin、自動 replan（每個 revision 最多一次）、`replan` annotation、`requireApproval` 流程
- 解決 secret ID 的 cache 問題（§6.5）
- plan 相關指標

**驗收**：第一次建置會產生 plan 並推上 registry；第二次（不同 commit、依賴沒變）不呼叫 LLM 而且命中 cache；故意破壞建置後會自動重新規劃一次並成功。

### M5：強化與文件
- NetworkPolicy、`hostUsers: false` 的預設值與退回選項
- ServiceMonitor、PrometheusRule
- Helm chart 完整化
- 使用者文件、維運文件（registry TLS、GC、buildkitd 容量規劃）

---

## 16. 待驗證清單

**〔變更〕** 新增「結果」欄；細節與來源見 [feasibility.md](feasibility.md)。

| # | 項目 | 影響範圍 | 結果 |
|---|---|---|---|
| 1 | zbplan 的輸出方式、exit code、可讀取的輪數與 token 資訊 | §6.2、§10.2 | 已驗證：stdout、exit 0／1；CLI 無輪數與 token，需上游支援 |
| 2 | zbplan `pkg/buildenv` 能否從外部傳入 namespace key | §6.5 | 已驗證：不行，需上游支援 |
| 3 | zbplan 的 LLM provider 設定方式與授權（MPL-2.0） | §6.2 | 已驗證：`ZBPLAN_*` 環境變數；以 binary 散布即可 |
| 4 | Railpack frontend 的 opts（`secrets-hash`、`cache-key`）在目前版本的確切名稱 | §6.5 | 已驗證：`build-arg:secrets-hash`、`build-arg:cache-key` |
| 5 | Keel native webhook 的 endpoint 與 payload 格式 | §9 | 已驗證：`POST /v1/webhooks/native` :9300，`{"name","tag"}` |
| 6 | Flux Receiver 用於觸發 ImageRepository 的設定方式 | §9 | 已驗證：`generic-hmac`，`X-Signature` |
| 7 | buildkitd readiness probe 的最佳做法 | §7.1 | 已驗證：exec `buildctl debug workers`（本機 socket） |
| 8 | go-github 與 ghinstallation 的最新 API | §5.2 | 改用 octocrab 0.54.5；GHES 未驗證 |
| 9 | BuildKit Go client 中 attestation 與 session auth 的 API | §6.4 | 不適用：改用 `buildctl` 的 `--opt attest:*` 與 `DOCKER_CONFIG` |
| 10 | 目標叢集的 Kubernetes 與 containerd 版本（是否支援 `hostUsers: false`） | §7.2 | 需求已確認（K8s ≥ 1.33、containerd ≥ 2.0、Linux ≥ 6.3）；目標叢集版本仍待確認 |
| 11 | **〔新增〕** zbplan 連 buildkitd 的 mTLS | §6.3 | 已驗證：不支援，需上游支援 |
| 12 | **〔新增〕** 每個 namespace 一個 buildkitd 的隔離 | §1.3、§7 | 已驗證（k3s v1.36，見 [poc.md](poc.md)）：兩個 namespace 同時在各自的 buildkitd 建置；A 的 client 憑證被 B 拒絕；A 的帳號推不進 B 的路徑；`RUN` 連不到其他 namespace 的 Service、Kubernetes API 與 `169.254.169.254`，連得到外網與 registry；`RUN --network=host` 與名單外的 frontend 被拒絕 |
| 13 | **〔新增〕** 冷啟動與閒置成本 | §7.1 | 已驗證：新 PVC 約 8 秒、既有 PVC 約 5 秒進入建置；閒置 buildkitd 約 22 MiB anonymous memory、45 MiB working set |
| 14 | **〔新增〕** `privileged` buildkitd 在 gVisor、Kata 下能否運作 | §7.2 | 未驗證 |

---

## 17. 設計決策紀錄（ADR 摘要）

**ADR-1：不使用 Tekton 或 Shipwright**
Shipwright 的資源模型（Build、BuildRun、BuildStrategy）很接近我們的需求，但它依賴 Tekton。Tekton 是通用 pipeline 引擎，需要注入 entrypoint 才能在單一 Pod 中依序執行多個 step，還要額外處理 sidecar 停止、結果大小限制、workspace PVC 親和性等問題，控制平面大約有十個 Deployment。我們的範圍只有 source → image，一個 Job 就能完成。為了降低使用者遷移成本，API 命名盡量貼近 Shipwright。

**ADR-2：每次建置使用 Job，而不是在 controller 內直接呼叫 BuildKit**
若在 controller 內呼叫，controller 重啟時進行中的 solve session 會中斷且無法重新接上，原始碼也必須經過 controller 傳輸，zbplan 的 LLM 迴圈還會與 reconcile 搶資源。使用 Job 的話，每次建置有獨立的生命週期、log 與資源限制，controller 也能保持無狀態。

**ADR-3：Webhook 只是喚醒訊號**
GitHub 不保證 webhook 的送達順序，force push 會讓 payload 中的 SHA 失效，失敗的 webhook 也不會自動重送。因此 reconciler 一律自行解析 ref 取得 SHA（level-triggered），並保留低頻 polling 作為安全網。

**ADR-4：Plan 存成 OCI artifact**
內容定址、不佔用 etcd、與 image 一起被 registry GC 管理，也讓使用者可以直接取出 Dockerfile 進行 eject。

**ADR-5：zbplan 採用「先沿用，失敗才重新規劃」**
AI 規劃的結果不具確定性，若每次建置都重新規劃，等於讓 LLM 每次都重新決定 production 長什麼樣。沿用已 pin 的 plan 可以確保可重現性，也讓 LLM 費用趨近於零；只在建置失敗時自動重新規劃一次。

**ADR-6：不使用 admission webhook server**
使用 CRD 的 CEL 規則處理不可變性，用 ValidatingAdmissionPolicy 處理 namespace 輸出路徑限制，兩者都是 API server 內建，不需要維運 webhook 的憑證與可用性。

**ADR-7：不負責 log 保存**
交給使用者既有的 log collector。我們只確保 Pod 保留時間足夠、label 齊全，並提供 log URL 樣板。

**ADR-8：~~buildkitd 共用池加上 rendezvous hashing~~ 〔變更〕每個 namespace 一個 buildkitd**
`RUN --mount=type=cache` 的內容不會隨 `--cache-to type=registry` 匯出，而 Railpack 和 zbplan 都大量依賴 cache mount。若每次建置都使用新的 buildkitd，就幾乎無法利用這些 cache，所以 buildkitd 仍是長期存在、有 PVC 的 StatefulSet。

原本的設計是全叢集共用一個池，依 repo 以 rendezvous hashing 分配，並假設使用者都是互相信任的內部團隊。現在不同 namespace 可能是不同團隊，而在共用的 buildkitd 中，一個建置的 `RUN` 只要用相同的 id 就能掛上別人的 cache mount，逃出容器後還讀得到別人的原始程式碼與 secret，所以改成每個 namespace 一個 buildkitd，各自有 CA、registry 帳號與 cache。代價：

- 不再跨 namespace 共用 cache；同一個 namespace 內仍然共用。
- 每個 namespace 多一個 Pod 與一個 PVC。閒置時縮到 0（PVC 保留），所以常駐成本只有 PVC；下一次建置多等 5 到 8 秒的冷啟動。
- controller 要管理 StatefulSet、Service、NetworkPolicy 與憑證，RBAC 也多了這些權限（§12）。

考慮過但沒採用：

- **每次建置一個 buildkitd**：隔離最好，但 cache mount 每次都是空的。
- **維持共用池，只加 mTLS 與 registry 隔離**：`RUN` 仍在同一個 daemon 中執行，跨 namespace 讀取 cache mount 的問題還在。
- **預設使用 gVisor**：sandbox 才擋得住刻意的逃逸，但會讓建置變慢，也不是每個叢集都有；改成選用的 `runtimeClassName`（§7.2）。

---

**ADR-9（〔新增〕）：controller 用 Rust，Job 用原生 `buildctl` 加 Rust 包裝程式**
原始計畫假設 Go（controller-runtime、BuildKit Go client）。我們改用與 Zeabur Identity 相同的 Rust 範本：同一套 lint、供應鏈政策（cargo-deny、最短發布時間）、CI 與 crate 分層，維護者不必在兩種語言之間切換。代價在 Job：Rust 目前沒有能上正式環境的 BuildKit client，現有的只是原型，或缺少檔案同步；自己寫一個約需三到五人週，還要持續追上游協定。`buildctl` 已提供 Job 需要的一切（frontend、local、secret、registry cache、mTLS、`--metadata-file`、`--progress rawjson`），所以 Job 直接執行它，Rust 包裝程式只負責解析輸出與寫 termination message。`buildctl` 參數由 controller 以純函式組出並以 golden test 固定，升級 BuildKit 時差異一目了然。若之後出現維護良好的 Rust client，可以只替換包裝程式。

## 18. 參考資料

- Shipwright：<https://github.com/shipwright-io/build>
- kpack：<https://github.com/buildpacks-community/kpack>
- Railpack：<https://railpack.com/guides/running-railpack-in-production/>
- zbplan：<https://github.com/zeabur/zbplan>
- Flux Image Automation（2.7 GA）：<https://fluxcd.io/flux/components/image/>
- Flux Operator ResourceSet image automation：<https://fluxoperator.dev/docs/resourcesets/image-automation/>
- Keel：<https://keel.sh>
- BuildKit rootless 與 user namespace：<https://kubernetes.web.cern.ch/blog/2025/06/19/rootless-container-builds-on-kubernetes/>
- Repo2Run（NeurIPS 2025）：<https://arxiv.org/abs/2502.13681>
- Mokhov et al., *Build Systems à la Carte*（ICFP 2018）
- **〔新增〕** BuildKit `buildctl build`：<https://github.com/moby/buildkit/blob/v0.34.1/cmd/buildctl/build.go>
- **〔新增〕** Kubernetes user namespaces：<https://kubernetes.io/docs/concepts/workloads/pods/user-namespaces/>
- **〔新增〕** BuildKit `buildkitd.toml`（`networkMode`、`insecure-entitlements`、`allowedRepositories`）：<https://github.com/moby/buildkit/blob/v0.34.1/docs/buildkitd.toml.md>
- **〔新增〕** Zot：<https://zotregistry.dev>
