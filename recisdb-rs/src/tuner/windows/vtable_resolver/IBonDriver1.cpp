#include "../IBonDriver.hpp"

extern "C" {

    BOOL C_OpenTuner(IBonDriver* b) {
        try { return b->OpenTuner(); } catch (...) { return 0; }
    }
    void C_CloseTuner(IBonDriver* b) {
        try { b->CloseTuner(); } catch (...) {}
    }

    BOOL C_SetChannel(IBonDriver* b, BYTE ch) {
        try { return b->SetChannel(ch); } catch (...) { return 0; }
    }
    float C_GetSignalLevel(IBonDriver* b) {
        try { return b->GetSignalLevel(); } catch (...) { return -1.0f; }
    }

    DWORD C_WaitTsStream(IBonDriver* b, DWORD timeout) {
        try { return b->WaitTsStream(timeout); } catch (...) { return 0; }
    }
    DWORD C_GetReadyCount(IBonDriver* b) {
        try { return b->GetReadyCount(); } catch (...) { return 0; }
    }

    // 1) BYTE* 版
    BOOL C_GetTsStream(IBonDriver* b, BYTE* pDst, DWORD* pdwSize, DWORD* pdwRemain) {
        try { return b->GetTsStream(pDst, pdwSize, pdwRemain); } catch (...) { return 0; }
    }

    // 2) BYTE** 版
    BOOL C_GetTsStream2(IBonDriver* b, BYTE** ppDst, DWORD* pdwSize, DWORD* pdwRemain) {
        try { return b->GetTsStream(ppDst, pdwSize, pdwRemain); } catch (...) { return 0; }
    }

    void C_PurgeTsStream(IBonDriver* b) {
        try { b->PurgeTsStream(); } catch (...) {}
    }
    void C_Release(IBonDriver* b) {
        try { b->Release(); } catch (...) {}
    }

    IBonDriver* CreateBonDriver(); // BonDriver DLL の CreateBonDriver を別でリンク

}
