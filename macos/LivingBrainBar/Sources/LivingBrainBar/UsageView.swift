import Charts
import LivingBrainKit
import SwiftUI

struct UsageView: View {
    @ObservedObject var workspace: WorkspaceModel
    @State private var days = 14
    @State private var usage: Usage?
    @State private var error: APIError?
    @State private var loading = true

    var body: some View {
        content
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .navigationTitle("Usage")
            .navigationSubtitle(usage?.workspaceId.map { "Workspace \($0)" } ?? "")
            .toolbar {
                ToolbarItemGroup {
                    Picker("Range", selection: $days) {
                        Text("7 days").tag(7)
                        Text("14 days").tag(14)
                        Text("31 days").tag(31)
                    }
                    .pickerStyle(.segmented)
                    Button { load() } label: { Label("Reload", systemImage: "arrow.clockwise") }
                }
            }
            .task(id: days) { load() }
    }

    @ViewBuilder private var content: some View {
        if loading && usage == nil {
            StatePlaceholder(kind: .loading, title: "Loading usage…")
        } else if case let .forbidden(message)? = error {
            StatePlaceholder(kind: .empty, title: "Usage is for workspace admins",
                             message: message + " Ask a workspace owner, or sign in with a full-access account (scoped tokens are never admin).",
                             systemImage: "lock")
        } else if let error, usage == nil {
            StatePlaceholder(kind: .error, title: "Could not load usage", message: error.localizedDescription, retry: load)
        } else if let usage {
            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    tiles(usage)
                    budget(usage)
                    chartCard("Model requests per day") {
                        Chart(usage.days) { day in
                            BarMark(x: .value("Day", day.date ?? .distantPast, unit: .day),
                                    y: .value("Requests", day.modelRequests))
                                .foregroundStyle(Theme.accent)
                        }
                    }
                    chartCard("Tokens per day") {
                        Chart {
                            ForEach(usage.days) { day in
                                BarMark(x: .value("Day", day.date ?? .distantPast, unit: .day),
                                        y: .value("Tokens", day.promptTokens))
                                    .foregroundStyle(by: .value("Kind", "Prompt"))
                                BarMark(x: .value("Day", day.date ?? .distantPast, unit: .day),
                                        y: .value("Tokens", day.completionTokens))
                                    .foregroundStyle(by: .value("Kind", "Completion"))
                            }
                        }
                        .chartForegroundStyleScale(["Prompt": Theme.accent, "Completion": Color.teal.opacity(0.6)])
                    }
                }
                .padding(24)
            }
        } else {
            StatePlaceholder(kind: .loading, title: "Loading usage…")
        }
    }

    private func tiles(_ usage: Usage) -> some View {
        let totals = usage.totals
        return HStack(spacing: 12) {
            Tile(title: "Model requests", value: (totals?.modelRequests ?? 0).formatted())
            Tile(title: "Prompt tokens", value: (totals?.promptTokens ?? 0).compact)
            Tile(title: "Completion tokens", value: (totals?.completionTokens ?? 0).compact)
            Tile(title: "Emails sent", value: (totals?.emailsSent ?? 0).formatted())
            Tile(title: "Model cost", value: totals?.modelUsdMicros.map { (Double($0) / 1_000_000).formatted(.currency(code: "USD").precision(.fractionLength(2...4))) } ?? "—")
        }
    }

    @ViewBuilder private func budget(_ usage: Usage) -> some View {
        if let spent = usage.monthToDateUSD {
            VStack(alignment: .leading, spacing: 8) {
                HStack {
                    Text("Month to date").font(.headline)
                    Spacer()
                    Label(usage.isOverBudget ? "Over budget" : "Within budget",
                          systemImage: usage.isOverBudget ? "exclamationmark.octagon.fill" : "checkmark.seal.fill")
                        .foregroundStyle(usage.isOverBudget ? .red : Theme.accent)
                        .font(.callout.weight(.medium))
                }
                if let fraction = usage.budgetFraction {
                    ProgressView(value: min(fraction, 1))
                        .tint(usage.isOverBudget ? .red : Theme.accent)
                }
                Text("\(spent.formatted(.currency(code: "USD"))) spent" +
                     (usage.budgetUSD.map { " of \($0.formatted(.currency(code: "USD"))) budget" } ?? ""))
                    .font(.callout).foregroundStyle(.secondary)
            }
            .padding(16)
            .background(RoundedRectangle(cornerRadius: 12).fill(Color.secondary.opacity(0.07)))
        }
    }

    private func chartCard<C: View>(_ title: String, @ViewBuilder chart: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(title).font(.headline)
            chart().frame(height: 200)
        }
        .padding(16)
        .background(RoundedRectangle(cornerRadius: 12).fill(Color.secondary.opacity(0.07)))
    }

    private func load() {
        loading = true
        let range = days
        Task {
            do {
                let loaded = try await workspace.call { try await $0.usage(days: range) }
                usage = loaded
                error = nil
                workspace.workspaceId = loaded.workspaceId
            } catch let apiError as APIError {
                error = apiError
                if case .forbidden = apiError { usage = nil }
            } catch {
                self.error = .transport(error.localizedDescription)
            }
            loading = false
        }
    }
}

struct Tile: View {
    let title: String
    let value: String

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.caption).foregroundStyle(.secondary)
            Text(value).font(.title2.weight(.semibold)).monospacedDigit()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(14)
        .background(RoundedRectangle(cornerRadius: 12).fill(Color.secondary.opacity(0.07)))
    }
}
