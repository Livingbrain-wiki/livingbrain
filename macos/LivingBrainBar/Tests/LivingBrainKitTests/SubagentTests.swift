import XCTest
@testable import LivingBrainKit

/// Version-3 records: a subagent's whole session nested in the call that
/// launched it, recursively, and the model named on a turn where it changes.
final class SubagentTests: XCTestCase {
    func testShowDecodesNestedSubagentsFromAVersion3Record() throws {
        let shown = try CLIOutput.sessionShow(fixture("logs-show-v3"))
        let record = try XCTUnwrap(shown.record)
        XCTAssertEqual(record.schemaVersion, 3)
        XCTAssertEqual(record.mainModel, "claude-opus-4-5")
        let call = try XCTUnwrap(record.turns[1].calls.first)
        XCTAssertTrue(call.hasDetail)
        let sub = try XCTUnwrap(call.subagent)
        XCTAssertEqual(sub.name, "Explore")
        XCTAssertEqual(sub.description, "Survey logging")
        XCTAssertEqual(sub.models, ["claude-haiku-4-5"])
        XCTAssertEqual(sub.status, "completed")
        XCTAssertEqual(sub.totals.all, 225)
        XCTAssertEqual(sub.header, "Explore · claude-haiku-4-5 · 1m 23s · 225 tokens · completed")
        XCTAssertEqual(sub.finalAnswer, "Two loggers: src/a.rs and src/b.rs.")
        XCTAssertEqual(sub.steps.count, 3, "every turn but the answer")
        let nested = try XCTUnwrap(sub.turns[2].calls.first?.subagent)
        XCTAssertEqual(nested.name, "general-purpose")
        XCTAssertEqual(record.subagents.map(\.name), ["Explore", "general-purpose"])
        // The main agent switches model on its last turn: one badge, there.
        let badges = SessionRecord.modelBadges(record.turns, startingWith: record.mainModel)
        XCTAssertEqual(badges, [record.turns.count - 1: "claude-sonnet-4-5"])
    }

    func testOlderRecordsStillDecodeWithoutSubagents() throws {
        let shown = try CLIOutput.sessionShow(fixture("logs-show-v2"))
        let record = try XCTUnwrap(shown.record)
        XCTAssertTrue(record.subagents.isEmpty)
        XCTAssertEqual(record.omittedSubagentTurns, 0)
        XCTAssertNil(record.turns.first?.model)
    }

    func testFormatting() {
        XCTAssertEqual(SubagentFormat.duration(45), "45s")
        XCTAssertEqual(SubagentFormat.duration(185), "3m 05s")
        XCTAssertEqual(SubagentFormat.duration(3720), "1h 02m")
        XCTAssertNil(SubagentFormat.duration(-1))
        XCTAssertEqual(SubagentFormat.grouped(1_234_567), "1,234,567")
        XCTAssertEqual(SubagentFormat.grouped(12), "12")
    }
}
