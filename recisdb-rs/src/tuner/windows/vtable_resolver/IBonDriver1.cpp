#include "../IBonDriver.hpp"

extern "C" {

    BOOL C_OpenTuner(IBonDriver* b) { return b->OpenTuner(); }
    void C_CloseTuner(IBonDriver* b) { b->CloseTuner(); }

    BOOL C_SetChannel(IBonDriver* b, BYTE ch) { return b->SetChannel(ch); }
    float C_GetSignalLevel(IBonDriver* b) { return b->GetSignalLevel(); }

    DWORD C_WaitTsStream(IBonDriver* b, DWORD timeout) { return b->WaitTsStream(timeout); }
    DWORD C_GetReadyCount(IBonDriver* b) { return b->GetReadyCount(); } // ←必須 [2](https://support.rockwellautomation.com/app/answers/answer_view/a_id/1153049/~/studio-5000-logix-designer-error-0xc0000005-on-windows-11-24h2-)

    // 1) BYTE* 版
    BOOL C_GetTsStream(IBonDriver* b, BYTE* pDst, DWORD* pdwSize, DWORD* pdwRemain) {
        return b->GetTsStream(pDst, pdwSize, pdwRemain);
    }

    // 2) BYTE** 版
    BOOL C_GetTsStream2(IBonDriver* b, BYTE** ppDst, DWORD* pdwSize, DWORD* pdwRemain) {
        return b->GetTsStream(ppDst, pdwSize, pdwRemain);
    }

    void C_PurgeTsStream(IBonDriver* b) { b->PurgeTsStream(); }
    void C_Release(IBonDriver* b) { b->Release(); }

    IBonDriver* CreateBonDriver(); // BonDriver DLL の CreateBonDriver を別でリンク

}
