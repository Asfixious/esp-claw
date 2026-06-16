struct OrchestratorInstance{
    // api key
    llm: Arc<ClawApi>,
    conversation_agent: ConversationAgent,
}