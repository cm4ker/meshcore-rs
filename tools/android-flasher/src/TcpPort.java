package ru.ommesh.otg;
import java.net.*;
import java.io.*;

final class TcpPort implements Ota.Port, Closeable {
    final Socket socket=new Socket();
    TcpPort(String address) throws IOException {
        try {
            URI u=new URI("tcp://"+address.trim());
            if(u.getHost()==null||u.getUserInfo()!=null||!u.getPath().isEmpty()||u.getQuery()!=null||u.getFragment()!=null)throw new IOException("Введите адрес устройства и порт.");
            int port=u.getPort()==-1?5000:u.getPort();
            socket.connect(new InetSocketAddress(u.getHost(),port),8000);
            socket.setTcpNoDelay(true);
        } catch(Exception e){socket.close();throw new IOException("Не удалось подключиться: "+e.getMessage(),e);}
    }
    public void write(byte[] b)throws IOException{socket.getOutputStream().write(b);}
    public int read(byte[] b,int ms)throws IOException{
        socket.setSoTimeout(Math.max(1,ms));
        try{int n=socket.getInputStream().read(b);if(n<0)throw new EOFException("Устройство закрыло соединение.");return n;}
        catch(SocketTimeoutException e){return 0;}
    }
    public void close()throws IOException{socket.close();}
}
