package stirling.software.SPDF.model.api.security;

import static org.assertj.core.api.Assertions.assertThat;

import org.junit.jupiter.api.Test;

class PasswordRequestLoggingTest {
    @Test
    void prepareDoesNotLogItsPassword() {
        var request = new PDFPasswordRequest();
        request.setPassword("prepare-secret");

        assertThat(request.toString()).doesNotContain("prepare-secret");
        assertThat(request.getPassword()).isEqualTo("prepare-secret");
    }
}
