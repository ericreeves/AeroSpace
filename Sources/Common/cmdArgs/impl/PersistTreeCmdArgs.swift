public struct PersistTreeCmdArgs: CmdArgs {
    /*conforms*/ public var commonState: CmdArgsCommonState
    public init(rawArgs: StrArrSlice) { self.commonState = .init(rawArgs) }
    public static let parser: CmdParser<Self> = .init(
        kind: .persistTree,
        help: persist_tree_help_generated,
        flags: [:],
        posArgs: [],
    )
}
