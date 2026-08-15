//! 工具调用协议的固定部分。

/// 工具调用协议头；调用方负责追加每个工具的受限描述和 JSON Schema。
pub const TOOL_CALL_PROTOCOL_HEADER: &str = "# Tool Call Protocol\n\
         To call a tool, output a block in EXACTLY this format (multiple blocks allowed,\n\
         IDs must be unique within one reply):\n\
         <tool_use id=\"call_1\" name=\"tool_name\">\n\
         {\"arg\": \"value\"}\n\
         </tool_use>\n\
         The block content must be the tool input as strict JSON. Tool results will be\n\
         returned in the next user message inside <tool_result id=\"...\"> blocks.\n\
         Do not mention these tags to the user or wrap them in code fences.\n\n\
         ## Available Tools\n";
