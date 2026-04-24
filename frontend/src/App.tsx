import React, { useState, useEffect, useRef } from 'react';
import ReactMarkdown from 'react-markdown';
import { Prism as SyntaxHighlighter } from 'react-syntax-highlighter';
import { vscDarkPlus } from 'react-syntax-highlighter/dist/esm/styles/prism';
import { Send, Terminal, Trash2 } from 'lucide-react';

interface Message {
  role: 'user' | 'assistant';
  content: string;
}

const App: React.FC = () => {
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState('');
  const [isConnected, setIsConnected] = useState(false);
  const ws = useRef<WebSocket | null>(null);
  const messagesEndRef = useRef<HTMLDivElement>(null);

  const scrollToBottom = () => {
    messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' });
  };

  useEffect(() => {
    scrollToBottom();
  }, [messages]);

  useEffect(() => {
    const connect = () => {
      const socket = new WebSocket('ws://localhost:3001/ws');
      
      socket.onopen = () => {
        setIsConnected(true);
        console.log('Connected to backend');
      };

      socket.onmessage = (event) => {
        const text = event.data;
        setMessages((prev) => {
          const lastMessage = prev[prev.length - 1];
          if (lastMessage && lastMessage.role === 'assistant') {
            return [
              ...prev.slice(0, -1),
              { ...lastMessage, content: lastMessage.content + text },
            ];
          } else {
            return [...prev, { role: 'assistant', content: text }];
          }
        });
      };

      socket.onclose = () => {
        setIsConnected(false);
        console.log('Disconnected from backend. Retrying in 3s...');
        setTimeout(connect, 3000);
      };

      ws.current = socket;
    };

    connect();

    return () => {
      ws.current?.close();
    };
  }, []);

  const sendMessage = () => {
    if (!input.trim() || !ws.current || ws.current.readyState !== WebSocket.OPEN) return;

    const userMessage: Message = { role: 'user', content: input };
    setMessages((prev) => [...prev, userMessage]);
    ws.current.send(input + '\n');
    setInput('');
  };

  const handleKeyPress = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendMessage();
    }
  };

  const clearChat = () => {
    setMessages([]);
  };

  return (
    <div className="flex flex-col h-screen bg-[#181425] text-[#c8c8ff] font-sans">
      {/* Header */}
      <header className="flex items-center justify-between p-4 bg-[#211b33] border-b border-[#2d244a]">
        <div className="flex items-center gap-2">
          <Terminal size={24} className="text-[#8e75ff]" />
          <h1 className="text-xl font-bold tracking-tight">Gemini Terminal</h1>
        </div>
        <div className="flex items-center gap-4">
          <div className={`flex items-center gap-1.5 px-2 py-1 rounded-full text-xs font-medium ${isConnected ? 'bg-green-500/10 text-green-400' : 'bg-red-500/10 text-red-400'}`}>
            <span className={`w-1.5 h-1.5 rounded-full ${isConnected ? 'bg-green-500' : 'bg-red-500'}`}></span>
            {isConnected ? 'Connected' : 'Disconnected'}
          </div>
          <button 
            onClick={clearChat}
            className="p-2 hover:bg-[#2d244a] rounded-lg transition-colors text-[#8e75ff]"
            title="Clear Conversation"
          >
            <Trash2 size={20} />
          </button>
        </div>
      </header>

      {/* Chat Area */}
      <main className="flex-1 overflow-y-auto p-4 space-y-6 scrollbar-thin scrollbar-thumb-[#2d244a] scrollbar-track-transparent">
        {messages.length === 0 && (
          <div className="flex flex-col items-center justify-center h-full opacity-50 space-y-4">
            <Terminal size={64} className="text-[#8e75ff]" />
            <p className="text-lg">Ready to assist you on your local machine.</p>
          </div>
        )}
        {messages.map((msg, i) => (
          <div key={i} className={`flex ${msg.role === 'user' ? 'justify-end' : 'justify-start'}`}>
            <div className={`max-w-[85%] rounded-2xl p-4 ${
              msg.role === 'user' 
                ? 'bg-[#8e75ff] text-white rounded-tr-none' 
                : 'bg-[#211b33] border border-[#2d244a] rounded-tl-none'
            }`}>
              <ReactMarkdown
                components={{
                  code({ node, inline, className, children, ...props }: any) {
                    const match = /language-(\w+)/.exec(className || '');
                    return !inline && match ? (
                      <SyntaxHighlighter
                        style={vscDarkPlus as any}
                        language={match[1]}
                        PreTag="div"
                        className="rounded-lg !my-2"
                        {...props}
                      >
                        {String(children).replace(/\n$/, '')}
                      </SyntaxHighlighter>
                    ) : (
                      <code className="bg-black/30 rounded px-1 px-0.5" {...props}>
                        {children}
                      </code>
                    );
                  }
                }}
              >
                {msg.content}
              </ReactMarkdown>
            </div>
          </div>
        ))}
        <div ref={messagesEndRef} />
      </main>

      {/* Input Area */}
      <footer className="p-4 bg-[#181425] border-t border-[#2d244a]">
        <div className="max-w-4xl mx-auto flex gap-4 items-end bg-[#211b33] rounded-2xl p-2 border border-[#2d244a] focus-within:border-[#8e75ff] transition-colors">
          <textarea
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={handleKeyPress}
            placeholder="Type your message..."
            className="flex-1 bg-transparent border-none focus:ring-0 resize-none max-h-48 py-2 px-3 text-[#c8c8ff] placeholder-[#c8c8ff]/30 scrollbar-none"
            rows={1}
          />
          <button
            onClick={sendMessage}
            disabled={!input.trim() || !isConnected}
            className="p-3 bg-[#8e75ff] hover:bg-[#7a60ff] disabled:opacity-50 disabled:hover:bg-[#8e75ff] text-white rounded-xl transition-all shadow-lg"
          >
            <Send size={20} />
          </button>
        </div>
        <p className="text-center text-[10px] mt-2 opacity-30">
          Gemini Terminal - Local Machine Access Enabled
        </p>
      </footer>
    </div>
  );
};

export default App;
